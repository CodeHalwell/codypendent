/*---------------------------------------------------------------------------------------------
 * Copyright (c) Microsoft Corporation. All rights reserved.
 * Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/
import type { AgentMode, EventBody, SessionStoreSnapshot } from './index.js';

export interface TranscriptRow { id: string; kind: 'user' | 'assistant' | 'status' | 'patch'; text: string; timestamp?: string; runId?: string; modelId?: string }
export interface ApprovalCard { id: string; action: string; risk: string }
export interface QuestionCard { id: string; questions: Extract<EventBody, { type: 'QuestionAsked' }>['questions'] }
export interface SessionRunView {
	id: string;
	state: string;
	objective?: string;
	/** Recorded run mode; absent until its RunStarted history has been read. */
	mode?: AgentMode['type'];
	disposition?: Extract<EventBody, { type: 'RunCompleted' }>['disposition'];
	usage?: { inputTokens?: number; outputTokens?: number; costMicros?: number };
}
export interface SessionProjection {
	title: string;
	rows: TranscriptRow[];
	activeRuns: string[];
	approvals: ApprovalCard[];
	questions: QuestionCard[];
	historyTruncated: boolean;
	runs?: SessionRunView[];
}

/** Derived display state only; the daemon ledger remains authoritative. */
export function projectSession(snapshot: SessionStoreSnapshot, previous?: SessionProjection): SessionProjection {
	const rows = new Map<string, TranscriptRow>((previous?.rows ?? []).map(row => [row.id, row]));
	const activeRuns = new Set(previous?.activeRuns ?? snapshot.projection?.active_runs ?? []);
	const approvals = new Map<string, ApprovalCard>((previous?.approvals ?? []).map(item => [item.id, item]));
	for (const item of snapshot.projection?.pending_approvals ?? []) {
		approvals.set(item.approval_id, { id: item.approval_id, action: JSON.stringify(item.action), risk: item.risk.level.type });
	}
	const questions = new Map<string, QuestionCard>((previous?.questions ?? []).map(item => [item.id, item]));
	const runs = new Map<string, SessionRunView>((previous?.runs ?? []).map(item => [item.id, item]));
	for (const id of activeRuns) { if (!runs.has(id)) { runs.set(id, { id, state: 'Running' }); } }
	let title = snapshot.projection?.title ?? previous?.title ?? 'Shared Session';
	let truncated = previous?.historyTruncated === true || snapshot.droppedEvents > 0 || snapshot.projection !== undefined;
	let timestamp: string | undefined;
	const append = (id: string, kind: TranscriptRow['kind'], text: string): void => {
		const combined = (rows.get(id)?.text ?? '') + text;
		truncated ||= combined.length > 64_000;
		rows.set(id, { id, kind, text: combined.slice(-64_000), timestamp: rows.get(id)?.timestamp ?? timestamp });
	};
	for (const event of snapshot.events) {
		timestamp = event.occurred_at;
		const body = event.body;
		const id = String(event.sequence);
		switch (body.type) {
			case 'SessionCreated': title = body.title; break;
			case 'RunStarted': activeRuns.add(body.run_id); runs.set(body.run_id, { id: body.run_id, state: 'Running', objective: body.objective, mode: body.mode.type }); append(id, 'user', body.objective); break;
			case 'ModelStreamDelta':
				if (!body.thought) {
					const modelId = event.actor.type === 'Agent' && event.actor.run_id === body.run_id && event.actor.model.trim()
						? event.actor.model : undefined;
					const last = [...rows.values()].at(-1);
					const key = last?.kind === 'assistant' && last.modelId === modelId && (last.runId === body.run_id || last.id === `${body.run_id}:reply`)
						? last.id : rows.has(`${body.run_id}:reply`) ? `${body.run_id}:reply:${id}` : `${body.run_id}:reply`;
					append(key, 'assistant', body.text); rows.get(key)!.runId = body.run_id;
					if (modelId !== undefined) { rows.get(key)!.modelId = modelId; }
				}
				break;
			case 'NoteAppended': append(id, 'status', body.text); break;
			case 'RunStateChanged':
				runs.set(body.run_id, { ...runs.get(body.run_id), id: body.run_id, state: body.state.type });
				if (['Completed', 'Failed', 'Cancelled'].includes(body.state.type)) { activeRuns.delete(body.run_id); }
				else { activeRuns.add(body.run_id); }
				rows.set(`${body.run_id}:state`, { id: `${body.run_id}:state`, kind: 'status', text: body.state.type, timestamp });
				break;
			case 'RunCompleted':
				runs.set(body.run_id, { ...runs.get(body.run_id), id: body.run_id, state: body.disposition.type, disposition: body.disposition });
				activeRuns.delete(body.run_id);
				append(id, 'status', body.disposition.type === 'Failed' ? body.disposition.reason : body.disposition.type === 'Completed' ? 'Run finished · validation not assessed by this status' : body.disposition.type);
				break;
			case 'RunUsage':
				runs.set(body.run_id, { ...runs.get(body.run_id), id: body.run_id, state: runs.get(body.run_id)?.state ?? 'Unknown',
					usage: { inputTokens: body.prompt_tokens ?? undefined, outputTokens: body.completion_tokens ?? undefined, costMicros: body.cost_micros ?? undefined } });
				break;
			case 'ToolStarted': append(id, 'status', `${body.tool}${body.label ? ` · ${body.label}` : ''}`); break;
			case 'ToolCompleted': append(id, 'status', `${body.tool} · ${body.outcome.type === 'Failed' ? body.outcome.message : body.outcome.type}`); break;
			case 'ModelRetrying': append(id, 'status', `Retry ${body.attempt}/${body.max_attempts} · ${body.message}`); break;
			case 'CheckpointRecorded': append(id, 'status', `Checkpoint ${body.ordinal} · ${body.commit}`); break;
			case 'CheckpointRestored': append(id, 'status', body.restored ? `Checkpoint restored · ${body.checkpoint_id}` : `Checkpoint restore rolled back · ${body.checkpoint_id}`); break;
			case 'SessionForked': append(id, 'status', `Forked from ${body.from_session} at checkpoint ${body.checkpoint}`); break;
			case 'Unknown': append(id, 'status', 'Unsupported activity. Update the client to inspect this event.'); break;
			case 'ToolDenied': append(id, 'status', `Blocked by policy: ${JSON.stringify(body.action)}`); break;
			case 'ApprovalRequested': approvals.set(body.approval_id,
				{ id: body.approval_id, action: JSON.stringify(body.action), risk: body.risk.level.type }); break;
			case 'ApprovalResolved': approvals.delete(body.approval_id); break;
			case 'QuestionAsked': questions.set(body.question_id, { id: body.question_id, questions: body.questions }); break;
			case 'QuestionResolved': questions.delete(body.question_id); break;
			case 'PatchProposed': append(id, 'patch', body.preview
				? `${body.preview}${body.preview_truncated ? '\n[Preview truncated; review the full patch in Codypendent.]' : ''}`
				: `Patch proposed${body.files?.length ? `: ${body.files.join(', ')}` : ''}. Review the full patch in Codypendent.`); break;
			default: break;
		}
	}
	const retained = [...rows.values()].slice(-300);
	let characters = 0;
	for (let index = retained.length - 1; index >= 0; index--) {
		characters += retained[index].text.length;
		if (characters > 256_000) { retained.splice(0, index + 1); truncated = true; break; }
	}
	const recentRuns = new Set([...runs.keys()].slice(-64));
	return { title, rows: retained, activeRuns: [...activeRuns],
		runs: [...runs.values()].filter(run => activeRuns.has(run.id) || recentRuns.has(run.id)),
		approvals: [...approvals.values()], questions: [...questions.values()],
		historyTruncated: truncated || rows.size > 300 };
}
