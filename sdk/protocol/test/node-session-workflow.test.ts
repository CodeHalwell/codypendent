import { EventEmitter } from 'node:events';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { NodeSession, WorkflowCommandError } from '../src/node.js';
import { encodeEnvelope, FrameDecoder } from '../src/framing.js';
import { PROTOCOL_V1 } from '../src/version.js';
import type { Envelope, Payload, WorkflowCatalog } from '../src/index.js';

class WorkflowSocket extends EventEmitter {
	readonly written: Envelope[] = [];
	private readonly decoder = new FrameDecoder();
	write(bytes: Uint8Array): boolean { this.written.push(...this.decoder.push(bytes)); return true; }
	destroy(): void { this.emit('close'); }
	deliver(payload: Payload, correlationId?: string): void {
		this.emit('data', encodeEnvelope({ protocol_version: PROTOCOL_V1, message_id: 'fixture-reply', client_id: 'fixture', payload,
			...(correlationId ? { correlation_id: correlationId } : {}) }));
	}
	hello(minor = PROTOCOL_V1.minor): void {
		this.emit('connect');
		this.deliver({ type: 'ServerHello', selected_protocol: { ...PROTOCOL_V1, minor }, daemon_instance: 'fixture', daemon_version: 'fixture', build_id: 'fixture', heartbeat_interval_ms: 15_000 });
	}
	command(): Envelope & { payload: Extract<Payload, { type: 'Command' }> } {
		const request = this.written.at(-1)!;
		if (request.payload.type !== 'Command') { throw new Error('Expected a workflow command.'); }
		return request as Envelope & { payload: Extract<Payload, { type: 'Command' }> };
	}
	reply(payload: Payload): void { this.deliver(payload, this.command().message_id); }
}

const sessions: NodeSession[] = [];
afterEach(() => { for (const session of sessions.splice(0)) session.disconnect(); vi.useRealTimers(); });

async function fixture(canWrite = true, minor = PROTOCOL_V1.minor) {
	const sockets: WorkflowSocket[] = [];
	const session = new NodeSession({ canWrite: () => canWrite, onChange: () => {}, onError: () => {}, timeoutMs: 100,
		createConnection: () => { const socket = new WorkflowSocket(); sockets.push(socket); return socket; } });
	sessions.push(session);
	const connected = session.connect(); sockets[0].hello(minor); await connected;
	return { session, sockets, socket: sockets[0] };
}

const catalog: WorkflowCatalog = {
	repository: '/fixture/repo', truncated: false, diagnostics: [],
	items: [{ id: 'review', version: 1, description: 'Review code', definition_digest: 'a'.repeat(64), node_count: 2, source: { type: 'BuiltIn' }, inputs: { objective: { input_type: 'string', required: true } } }],
};

describe('unattached NodeSession workflow operations', () => {
	it('discovers authoritative metadata without creating or attaching a chat', async () => {
		const { session, socket } = await fixture();
		const result = session.listWorkflows('/fixture/repo');
		expect({ body: socket.command().payload.body, session: socket.command().session_id }).toEqual({ body: { type: 'ListWorkflows', repository: '/fixture/repo' }, session: undefined });
		socket.reply({ type: 'WorkflowCatalog', command_id: 'fixture', catalog });
		await expect(result).resolves.toEqual(catalog);
		expect(session.snapshot.sessionId).toBeUndefined();
		expect(socket.written.filter(frame => frame.payload.type === 'Command')).toHaveLength(1);
	});

	it('starts the named workflow with exact repository, inputs, and caller-owned idempotency', async () => {
		const { session, socket } = await fixture();
		const result = session.startWorkflow('review', { objective: 'Review this change', count: 2 }, '/fixture/repo', { idempotencyKey: 'stable-start' });
		expect({ body: socket.command().payload.body, key: socket.command().payload.idempotency_key, session: socket.command().session_id }).toEqual({
			body: { type: 'StartWorkflow', workflow_id: 'review', manifest: '', repository: '/fixture/repo', inputs: { objective: 'Review this change', count: 2 } },
			key: 'stable-start', session: undefined,
		});
		socket.reply({ type: 'WorkflowRunStarted', command_id: 'fixture', workflow_run_id: 'workflow-run' });
		await expect(result).resolves.toBe('workflow-run');
	});

	it('binds a start to the exact selected definition digest', async () => {
		const { session, socket } = await fixture();
		const digest = 'a'.repeat(64);
		const pending = session.startWorkflow('review', {}, '/fixture/repo', { idempotencyKey: 'selected-definition', expectedDefinitionDigest: digest });
		expect({ body: socket.command().payload.body, key: socket.command().payload.idempotency_key }).toEqual({
			body: { type: 'StartWorkflow', workflow_id: 'review', manifest: '', repository: '/fixture/repo', inputs: {}, expected_definition_digest: digest },
			key: 'selected-definition',
		});
		socket.reply({ type: 'WorkflowRunStarted', command_id: 'fixture', workflow_run_id: 'workflow-run' });
		await expect(pending).resolves.toBe('workflow-run');
	});

	it.each(['', 'a'.repeat(63), 'a'.repeat(65), 'A'.repeat(64), 'not-a-digest'])('rejects an invalid definition digest locally: %#', async digest => {
		const { session, socket } = await fixture();
		await expect(session.startWorkflow('review', {}, '/fixture/repo', { expectedDefinitionDigest: digest })).rejects.toMatchObject({
			code: 'workflow.invalid-request', definitiveRejection: false,
		});
		expect(socket.written.some(frame => frame.payload.type === 'Command')).toBe(false);
	});

	it('reads and controls the exact durable workflow without a dummy chat', async () => {
		const { session, socket } = await fixture();
		const snapshot = { workflow_run_id: 'workflow-run', phase: { type: 'Paused' as const }, nodes: [] };
		const reading = session.readWorkflowRun('workflow-run');
		socket.reply({ type: 'WorkflowRunSnapshot', command_id: 'fixture', snapshot });
		await expect(reading).resolves.toEqual(snapshot);
		const operations = [
			() => session.pauseWorkflow('workflow-run', { idempotencyKey: 'pause' }),
			() => session.resumeWorkflow('workflow-run', { idempotencyKey: 'resume' }),
			() => session.cancelWorkflow('workflow-run', { idempotencyKey: 'cancel' }),
			() => session.retryWorkflowNode('workflow-run', 'verify', { idempotencyKey: 'retry' }),
		];
		for (const operation of operations) {
			const pending = operation();
			socket.reply({ type: 'CommandAccepted', command_id: 'fixture' });
			await pending;
		}
		expect(socket.written.filter(frame => frame.payload.type === 'Command').map(frame => frame.payload.type === 'Command' ? frame.payload.body : null)).toEqual([
			{ type: 'ReadWorkflowRun', workflow_run_id: 'workflow-run' },
			{ type: 'PauseWorkflow', workflow_run_id: 'workflow-run' },
			{ type: 'ResumeWorkflow', workflow_run_id: 'workflow-run' },
			{ type: 'CancelWorkflow', workflow_run_id: 'workflow-run' },
			{ type: 'RetryWorkflowNode', workflow_run_id: 'workflow-run', node_id: 'verify' },
		]);
	});

	it('blocks every mutation locally when the workspace is not trusted', async () => {
		const { session, socket } = await fixture(false);
		for (const operation of [
			() => session.startWorkflow('review', {}, '/fixture/repo'), () => session.pauseWorkflow('workflow-run'),
			() => session.resumeWorkflow('workflow-run'), () => session.cancelWorkflow('workflow-run'), () => session.retryWorkflowNode('workflow-run', 'verify'),
		]) { await expect(operation()).rejects.toMatchObject({ name: 'WorkflowCommandError', code: 'workflow.workspace-untrusted', definitiveRejection: false }); }
		expect(socket.written.some(frame => frame.payload.type === 'Command')).toBe(false);
	});

	it.each(['CommandRejected', 'Error'] as const)('preserves structured %s evidence without claiming an uncertain outcome is a rejection', async type => {
		const { session, socket } = await fixture();
		const pending = session.startWorkflow('review', {}, '/fixture/repo').catch(error => error);
		socket.reply(type === 'CommandRejected'
			? { type, code: 'workflow.blocked', message: 'Workflow cannot start.', retryable: true, correlation_id: socket.command().message_id }
			: { type, code: 'workflow.blocked', message: 'Workflow cannot start.', retryable: true });
		const error = await pending;
		expect(error).toBeInstanceOf(WorkflowCommandError);
		expect(error).toMatchObject({ name: 'WorkflowCommandError', code: 'workflow.blocked', retryable: true, definitiveRejection: type === 'CommandRejected' });
	});

	it('rejects a stale launch result after navigation changes the captured connection', async () => {
		const { session, socket, sockets } = await fixture();
		const pending = session.startWorkflow('review', {}, '/fixture/repo', { idempotencyKey: 'one-start' }).catch(error => error);
		const oldRequest = socket.command();
		const connected = session.connect(); sockets[1].hello(); await connected;
		socket.deliver({ type: 'WorkflowRunStarted', command_id: 'fixture', workflow_run_id: 'old-run' }, oldRequest.message_id);
		expect(await pending).toMatchObject({ code: 'workflow.connection-changed', definitiveRejection: false });
		expect(sockets[1].written.some(frame => frame.payload.type === 'Command')).toBe(false);
	});

	it('rejects a snapshot attributed to a different workflow', async () => {
		const { session, socket } = await fixture();
		const pending = session.readWorkflowRun('workflow-run');
		socket.reply({ type: 'WorkflowRunSnapshot', command_id: 'fixture', snapshot: { workflow_run_id: 'different-run', phase: { type: 'Completed' }, nodes: [] } });
		await expect(pending).rejects.toMatchObject({ code: 'workflow.unexpected-response', definitiveRejection: false });
	});

	it('keeps timeout outcomes uncertain and never replays the launch', async () => {
		vi.useFakeTimers();
		const { session, socket } = await fixture();
		const pending = session.startWorkflow('review', {}, '/fixture/repo', { idempotencyKey: 'stable-start', timeoutMs: 10 }).catch(error => error);
		await vi.advanceTimersByTimeAsync(11);
		const error = await pending;
		expect(error.message).toContain('outcome may be unknown');
		expect(error.definitiveRejection).not.toBe(true);
		expect(socket.written.filter(frame => frame.payload.type === 'Command')).toHaveLength(1);
	});

	it('reports old-daemon discovery capability without sending an unsupported frame', async () => {
		const { session, socket } = await fixture(true, 11);
		await expect(session.listWorkflows('/fixture/repo')).rejects.toMatchObject({ code: 'workflow.catalog-unavailable', retryable: false });
		expect(socket.written.some(frame => frame.payload.type === 'Command')).toBe(false);
	});

	it('never lets an older daemon silently ignore a selected-definition precondition', async () => {
		const { session, socket } = await fixture(true, 11);
		await expect(session.startWorkflow('review', {}, '/fixture/repo', { expectedDefinitionDigest: 'a'.repeat(64) }))
			.rejects.toMatchObject({ code: 'workflow.definition-precondition-unavailable', retryable: false });
		expect(socket.written.some(frame => frame.payload.type === 'Command')).toBe(false);
	});

	it('keeps legacy starts without a digest compatible with an older daemon', async () => {
		const { session, socket } = await fixture(true, 11);
		const pending = session.startWorkflow('review', {}, '/fixture/repo');
		socket.reply({ type: 'WorkflowRunStarted', command_id: 'fixture', workflow_run_id: 'legacy-run' });
		expect(await pending).toBe('legacy-run');
	});
});
