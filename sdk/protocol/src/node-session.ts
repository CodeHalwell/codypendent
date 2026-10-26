/*---------------------------------------------------------------------------------------------
 * Copyright (c) Microsoft Corporation. All rights reserved.
 * Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/
import { NodeDaemonClient } from './node-client.js';
import type { AgentMode, CommandBody, ConnectionFactory, IdeContextUpdate, Payload, QuestionOutcome, RequestOptions, SessionSummary, SessionStoreSnapshot, SessionEvent } from './index.js';
import { projectSession, type SessionProjection } from './session-view.js';

export interface SharedSessionState {
	status: string;
	sessionId?: string;
	projection: SessionProjection;
}
const emptyProjection = (): SessionProjection => ({ title: 'Shared Sessions', rows: [], activeRuns: [],
	approvals: [], questions: [], historyTruncated: false });

export interface SharedSessionOptions {
	socketPath?: string;
	createConnection?: ConnectionFactory;
	canWrite: () => boolean;
	onChange: (state: SharedSessionState) => void;
	onError: (message: string) => void;
	timeoutMs?: number;
	clientName?: string;
	clientVersion?: string;
	onEvent?: (event: SessionEvent) => void;
}

/** Owns a connection, never an agent loop, credentials, tools, or run lifetime. */
export class SharedSession {
	private client?: NodeDaemonClient;
	private repository?: string;
	private hasRuns = false;
	private projectedSequence = 0;
	private baseProjection?: SessionStoreSnapshot['projection'];
	private restoration?: Promise<void>;
	private state: SharedSessionState = { status: 'closed', projection: emptyProjection() };
	private unsubscribe?: () => void;
	private rejectConnection?: (error: Error) => void;
	private restoring = false;
	private epoch = 0;
	private recovery = 0;
	constructor(private readonly options: SharedSessionOptions) { }

	get snapshot(): SharedSessionState { return this.state; }
	get repositoryRoot(): string | undefined { return this.repository; }

	async connect(session?: SessionSummary): Promise<void> {
		this.disconnect();
		const epoch = ++this.epoch;
		this.repository = session?.repository ?? undefined;
		this.hasRuns = !!session?.last_run_id;
		this.projectedSequence = 0;
		this.baseProjection = undefined;
		this.restoration = undefined;
		this.restoring = false;
		this.state = { status: 'connecting', sessionId: session?.session_id, projection: emptyProjection() };
		const client = new NodeDaemonClient({ socketPath: this.options.socketPath, sessionId: session?.session_id,
			createConnection: this.options.createConnection, clientName: this.options.clientName ?? 'codypendent-sdk', clientVersion: this.options.clientVersion ?? '1.1.0',
			role: { type: this.options.canWrite() ? 'Controller' : 'Observer' },
			requestTimeoutMs: this.options.timeoutMs ?? 15_000,
			capabilities: { rich_text: false, image_display: false, audio_capture: false, editor_mutations: false,
				diff_view: false, mouse: true, unicode: true, true_color: true, session_library: true } });
		this.client = client;
		client.on('status', status => { if (this.client === client) { this.state.status = status === 'attached' && this.restoring ? 'restoring' : status; this.publish(); } });
		client.on('serverHello', () => { if (this.client === client && !session) { this.state.status = 'ready'; this.publish(); } });
		client.on('event', event => { if (this.client === client) { this.options.onEvent?.(event); } });
		client.on('error', error => { if (this.client === client) { this.options.onError(error.message); } });
		client.on('commandRejected', error => { if (this.client === client) { this.options.onError(error.message); } });
		client.on('protocolError', error => { if (this.client === client) { this.options.onError(error.message); } });
		this.unsubscribe = client.store.subscribe(() => {
			if (this.client !== client) { return; }
			const snapshot = client.store.getSnapshot();
			const reset = snapshot.projection !== undefined && snapshot.projection !== this.baseProjection;
			this.state.projection = projectSession({ ...snapshot,
				events: snapshot.events.filter(event => reset || event.sequence > this.projectedSequence),
				projection: reset ? snapshot.projection : undefined }, reset ? undefined : this.state.projection);
			this.projectedSequence = snapshot.cursor;
			this.baseProjection = snapshot.projection;
			this.hasRuns ||= snapshot.events.some(event => event.body.type === 'RunStarted');
			if (reset) {
				const recovery = ++this.recovery;
				this.restoring = true;
				this.state.status = 'restoring';
				this.restoration = this.restoreHistory(client, snapshot.cursor, epoch, recovery);
				void this.restoration.catch(error => {
					if (this.client === client && this.recovery === recovery) { this.state.status = 'history-unavailable'; this.options.onError(error.message); this.publish(); }
				});
			}
			this.publish();
		});
		await new Promise<void>((resolve, reject) => {
			let finished = false;
			const finish = (error?: Error): void => {
				if (finished) { return; }
				finished = true;
				clearTimeout(timer);
				client.off('serverHello', hello);
				client.off('catchup', caughtUp);
				client.off('protocolError', protocolFailed);
				this.rejectConnection = undefined;
				if (error) { reject(error); } else { resolve(); }
			};
			const hello = (): void => { if (!session) { finish(); } };
			const caughtUp = (): void => {
				if (this.restoration) { void this.restoration.then(() => finish(), error => finish(error)); }
				else { finish(); }
			};
			const protocolFailed = (error: { message: string }): void => finish(new Error(error.message));
			const timer = setTimeout(() => finish(new Error('Cannot connect to Codypendent. Start it with codypendent daemon start, then reconnect.')),
				this.options.timeoutMs ?? 45_000);
			this.rejectConnection = error => finish(error);
			client.on('serverHello', hello);
			client.on('catchup', caughtUp);
			client.on('protocolError', protocolFailed);
			client.start();
		}).catch(error => { if (this.client === client) { this.disconnect(); } throw error; });
	}

	async listSessions(): Promise<SessionSummary[]> {
		const reply = await this.requireClient().requestGlobal({ type: 'SearchSessions', query: { query: '', limit: 100 } });
		this.checkReply(reply.payload);
		if (reply.payload.type !== 'SessionSearchResults') { throw new Error('Daemon returned an unexpected session search response.'); }
		return reply.payload.page.items.map(item => item.session).filter(session => !session.internal && !session.archived_at);
	}

	async createSession(title: string, workspace: string, repository: string, options?: RequestOptions & { internal?: boolean }): Promise<string> {
		this.requireWrite();
		const reply = await this.requireClient().requestGlobal({ type: 'CreateSession', title, workspace, repository, internal: options?.internal }, options);
		this.checkReply(reply.payload);
		if (reply.payload.type !== 'CommandAccepted' || !reply.session_id) { throw new Error('Daemon did not return a new session id.'); }
		const sessionId = reply.session_id;
		await this.connect({ session_id: sessionId, title, repository, state: 'active', created_at: '', updated_at: '' });
		return sessionId;
	}

	/** Close navigation without deleting the durable ledger or cancelling work. */
	async closeSession(): Promise<void> {
		this.requireWrite();
		if (!this.state.sessionId) { throw new Error('Open a shared session first.'); }
		await this.command({ type: 'CloseSession', session_id: this.state.sessionId });
	}

	async submit(text: string, mode: AgentMode['type'], options?: RequestOptions & { model?: string }): Promise<string | undefined> {
		this.requireWrite();
		if (!text.trim() || text.length > 100_000 || !['Ask', 'Explore', 'Plan', 'Build', 'Review'].includes(mode)) {
			throw new Error('Enter a prompt and choose a supported mode.');
		}
		const sessionId = this.state.sessionId;
		if (!sessionId) { throw new Error('Open or create a shared session first.'); }
		const reply = await this.command(this.hasRuns
			? { type: 'SubmitUserInput', session_id: sessionId, text, mode: { type: mode }, model: options?.model }
			: { type: 'StartRun', session_id: sessionId, objective: text, mode: { type: mode }, repository: this.repository, model: options?.model }, options);
		if (reply.type !== 'CommandAccepted') { throw new Error('Daemon did not acknowledge this submission.'); }
		this.hasRuns = true;
		return reply.type === 'CommandAccepted' ? reply.created_run ?? undefined : undefined;
	}

	async probeModels(): Promise<Extract<Payload, { type: 'ModelProbes' }>['models']> {
		const reply = await this.requireClient().requestGlobal({ type: 'ProbeModel', network: false });
		this.checkReply(reply.payload);
		if (reply.payload.type !== 'ModelProbes') { throw new Error('Daemon did not return model readiness.'); }
		return reply.payload.models;
	}

	async readService(body: Extract<CommandBody, { type: 'ReadCodeGraph' | 'ReadCodeGraphStatus' | 'ReadBlackboard' | 'ReadKnowledge' }>, options?: RequestOptions): Promise<Payload> {
		const reply = await this.requireClient().requestGlobal(body, options);
		this.checkReply(reply.payload); return reply.payload;
	}

	/** Scoped board mutations use the same authenticated daemon as native clients. */
	async writeBoard(body: Extract<CommandBody, { type: 'PostBlackboardItem' | 'UpdateBlackboardItem' }>, options?: RequestOptions): Promise<Payload> {
		this.requireWrite();
		const reply = await this.requireClient().requestGlobal(body, options);
		this.checkReply(reply.payload); return reply.payload;
	}

	async buildCodeGraph(repository: string): Promise<Payload> {
		this.requireWrite();
		const reply = await this.requireClient().requestGlobal({ type: 'BuildCodeGraph', repository }, { timeoutMs: 120_000 });
		this.checkReply(reply.payload); return reply.payload;
	}

	query(body: Extract<CommandBody, { type: 'ReadCodeGraph' | 'ReadCodeGraphStatus' | 'ReadSessionHistory' | 'ReadWorkflowRun' | 'ReadBlackboard' }>, options?: RequestOptions): Promise<Payload> { return this.command(body, options); }

	async resolveApproval(id: string, approve: boolean): Promise<void> {
		this.requireWrite();
		if (!this.state.projection.approvals.some(item => item.id === id)) { throw new Error('This approval is no longer pending.'); }
		await this.command({ type: 'ResolveApproval', approval_id: id, decision: { type: approve ? 'Approve' : 'Reject' }, scope: { type: 'Once' } });
	}

	async resolveQuestion(id: string, outcome: QuestionOutcome): Promise<void> {
		this.requireWrite();
		if (!this.state.projection.questions.some(item => item.id === id)) { throw new Error('This question is no longer pending.'); }
		await this.command({ type: 'ResolveQuestion', question_id: id, outcome });
	}

	async rememberMemory(text: string): Promise<void> {
		this.requireWrite();
		if (!this.state.sessionId || !text.trim() || text.length > 20_000) { throw new Error('Open a session and enter at most 20,000 characters.'); }
		const receipt = await this.command({ type: 'RememberMemory', session_id: this.state.sessionId, text });
		const sequence = receipt.type === 'CommandAccepted' ? receipt.sequence : undefined;
		if (typeof sequence !== 'number' || !Number.isSafeInteger(sequence) || sequence < 1) { throw new Error('Memory outcome is unknown. Inspect the session before retrying.'); }
		const result = await this.command({ type: 'ReadSessionEvents', session_id: this.state.sessionId, after_sequence: sequence - 1, limit: 1 });
		const event = result.type === 'SessionEventsPage' ? result.events.find(event => event.sequence === sequence) : undefined;
		if (event?.body.type !== 'NoteAppended') { throw new Error('Memory outcome is unknown. Inspect the session before retrying.'); }
		if (!event.body.text.startsWith('remembered: ')) { throw new Error(event.body.text); }
	}

	async cancelRun(id: string): Promise<void> {
		this.requireWrite();
		await this.command({ type: 'CancelRun', run_id: id });
	}

	updateEditorContext(update: IdeContextUpdate): void {
		if (this.options.canWrite() && this.state.status === 'attached') { this.client?.sendIdeContext(update); }
	}

	disconnect(): void {
		this.epoch++;
		this.rejectConnection?.(new Error('Connection superseded.'));
		this.unsubscribe?.(); this.unsubscribe = undefined;
		const previous = this.client; this.client = undefined;
		previous?.stop(); previous?.removeAllListeners();
		this.state.status = 'closed';
		this.publish();
	}

	private async restoreHistory(client: NodeDaemonClient, target: number, epoch: number, recovery: number): Promise<void> {
		let projection = emptyProjection();
		let cursor: string | undefined;
		let sequence = 0;
		let hasRuns = false;
		const deadline = Date.now() + 30_000;
		do {
			if (this.client !== client || this.epoch !== epoch || this.recovery !== recovery) { return; }
			if (Date.now() > deadline) { throw new Error('History recovery timed out. Reconnect to restore this session.'); }
			const reply = await client.request({ type: 'ReadSessionHistory', session_id: this.state.sessionId!, cursor, limit: 500 });
			this.checkReply(reply);
			if (reply.type !== 'SessionHistory') { throw new Error('Daemon could not restore session history.'); }
			const events = reply.page.items.filter(event => event.sequence <= target);
			for (const event of events) {
				if (event.sequence !== sequence + 1) { throw new Error('Session history has a gap. Reconnect before continuing.'); }
				sequence = event.sequence; hasRuns ||= event.body.type === 'RunStarted';
			}
			projection = projectSession({ ready: true, cursor: sequence, events, droppedEvents: 0 }, projection);
			cursor = reply.page.next_cursor ?? undefined;
			if (cursor && !events.length && sequence < target) { throw new Error('History recovery made no progress. Reconnect before continuing.'); }
		} while (cursor && sequence < target);
		if (this.client !== client || this.epoch !== epoch || this.recovery !== recovery) { return; }
		if (sequence !== target) { throw new Error('Session history is incomplete. Reconnect before continuing.'); }
		const snapshot = client.store.getSnapshot();
		const live = snapshot.events.filter(event => event.sequence > target);
		for (const event of live) {
			if (event.sequence !== sequence + 1) { throw new Error('Activity outpaced history recovery. Reconnect to catch up.'); }
			sequence = event.sequence; hasRuns ||= event.body.type === 'RunStarted';
		}
		this.state.projection = projectSession({ ...snapshot, projection: undefined, events: live }, projection);
		this.projectedSequence = sequence;
		this.hasRuns ||= hasRuns;
		this.restoring = false;
		this.state.status = 'attached';
		this.publish();
	}

	private async command(body: CommandBody, options?: RequestOptions): Promise<Payload> {
		if (this.state.status !== 'attached') { throw new Error('Wait for the session to reconnect and restore its history.'); }
		const result = await this.requireClient().request(body, options);
		this.checkReply(result);
		return result;
	}
	private checkReply(reply: Payload): void {
		if (reply.type === 'CommandRejected' || reply.type === 'Error') { throw new Error(reply.message); }
		if (reply.type === 'Unknown') { throw new Error('The daemon returned an unsupported response.'); }
	}
	private requireClient(): NodeDaemonClient {
		if (!this.client) { throw new Error('Connect to Codypendent first.'); }
		return this.client;
	}
	private requireWrite(): void { if (!this.options.canWrite()) { throw new Error('Trust this workspace before controlling an agent session.'); } }
	private publish(): void { this.options.onChange(this.state); }
}
