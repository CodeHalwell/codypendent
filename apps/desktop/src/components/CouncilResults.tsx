import { EmptyState, PageShell } from "./PageShell.js";
import React, { useCallback, useEffect, useRef, useState } from "react";
import { ArrowUpRight, Check, ChevronRight, Crown, Folder, LoaderCircle, RefreshCw, UsersRound } from "lucide-react";
import { renderMarkdown } from "../markdown.js";
import { relativeTime } from "../time.js";
import { modelLabel } from "../modelPresentation.js";
import { useLoadOnMount } from "../useLoadOnMount.js";
import { useCouncilRun, type CouncilRunController, type CouncilRunner } from "../useCouncilRun.js";
import { CouncilModel, councilTitle, repositoryName } from "./CouncilParticipants.js";
import type { CouncilResultCard, CouncilResultsPage } from "../localConfig.js";
import "./Councils.css";

export interface CouncilResultsProps {
  onLoad: () => Promise<CouncilResultsPage>;
  onRun?: CouncilRunner;
  runController?: CouncilRunController;
  initialCouncil?: string | null;
  councilNames?: string[];
  repository?: string | null | undefined;
  onOpenRepository?: () => void;
}
type Status = { kind: "loading" } | { kind: "loaded"; page: CouncilResultsPage } | { kind: "unavailable"; detail: string };
const phaseLabel: Record<string, string> = {
  "round-started": "Members are considering your question.", "member-completed": "Another perspective is ready.",
  "member-failed": "A member could not finish; the council is checking its remaining responses.",
  "chair-started": "The chair is bringing the findings together.", "warning": "The council has an update to review.",
};
const statuses: Record<string, string> = { completed: "Completed", "quorum-failed": "Not enough responses", "chair-failed": "Chair could not finish", "runtime-failed": "Interrupted" };

export const CouncilResults: React.FC<CouncilResultsProps> = ({ onLoad, onRun, runController, initialCouncil, councilNames, repository, onOpenRepository }) => {
  const [status, setStatus] = useState<Status>({ kind: "loading" });
  const localRun = useCouncilRun(onRun, initialCouncil ?? "");
  const councilRun = runController ?? localRun;
  const { council, objective, progress, running, failure: runFailure, result: runResult, setCouncil, setObjective } = councilRun;
  const generation = useRef(0);
  const seenCompletion = useRef(councilRun.completion);
  const [askAgainOpen, setAskAgainOpen] = useState(false);
  const load = useCallback(async () => {
    const current = ++generation.current; setStatus({ kind: "loading" });
    try { const page = await onLoad(); if (generation.current === current) setStatus({ kind: "loaded", page }); }
    catch (error) { if (generation.current === current) setStatus({ kind: "unavailable", detail: describe(error) }); }
  }, [onLoad]);
  useLoadOnMount(load);
  useEffect(() => { if (initialCouncil) setCouncil(initialCouncil); }, [initialCouncil, setCouncil]);
  useEffect(() => {
    if (seenCompletion.current !== councilRun.completion) {
      seenCompletion.current = councilRun.completion;
      setAskAgainOpen(false);
      void load();
    }
  }, [councilRun.completion, load]);
  const canRun = Boolean(onRun && repository && council.trim() && objective.trim() && !running);
  const run = () => { if (canRun && repository) void councilRun.start(repository); };
  const shownRepository = running ? councilRun.repository : repository;
  const latest = progress.at(-1);
  const saved = status.kind === "loaded" ? status.page.results.filter((result) => result.resultId !== runResult?.resultId) : [];
  const questionForm = onRun && <form className="council-card council-run-form" aria-label="Start a council" onSubmit={(event) => { event.preventDefault(); void run(); }}>
      <div className="council-run-top"><span><UsersRound size={19} aria-hidden="true" /></span><h2>What would you like a second opinion on?</h2></div>
      <label className="council-label"><span>Council</span>{councilNames ? <select className="council-input" data-testid="council-run-name" value={council} disabled={running} onChange={(event) => setCouncil(event.target.value)}>
        <option value="">Choose a council</option>{council && !councilNames.includes(council) && <option value={council}>{councilTitle(council)}</option>}
        {councilNames.map((name) => <option key={name} value={name}>{councilTitle(name)}</option>)}
      </select> : <input className="council-input" data-testid="council-run-name" value={council} disabled={running} placeholder="Choose a council" onChange={(event) => setCouncil(event.target.value)} />}</label>
      <textarea className="council-input" data-testid="council-run-objective" aria-label="Question for the council" value={objective} disabled={running} rows={3} onChange={(event) => setObjective(event.target.value)} placeholder="Describe the decision, review or problem you want your council to consider…" />
      <div className="council-run-footer"><div className="council-note">{shownRepository ? <><Folder size={12} aria-hidden="true" /> Working in {repositoryName(shownRepository)}</> : shownRepository === undefined ? "Reading the repository selection…" : "Choose a repository for the council to work in."}</div>
        {shownRepository === null && onOpenRepository && <button type="button" className="council-button" onClick={onOpenRepository}>Choose repository</button>}
        <button type="submit" className="council-button primary" disabled={!canRun} data-testid="council-run-submit">{running ? "Council is working…" : "Start council"}{!running && <ArrowUpRight size={14} aria-hidden="true" />}</button>
      </div>
    </form>;
  return <PageShell className="councils-page"><div className="councils-content">
    <header className="councils-header"><div><div className="councils-eyebrow"><UsersRound size={14} aria-hidden="true" /> A shared answer</div>
      <h1>Council results</h1><p>Put a question to your team, follow the discussion and return to what they found.</p></div></header>
    {questionForm && (runResult?.status === "completed" && !running
      ? <details className="council-details council-ask-again" open={askAgainOpen} onToggle={(event) => setAskAgainOpen(event.currentTarget.open)}>
          <summary>Ask another question</summary>{questionForm}
        </details>
      : questionForm)}
    {running && <section className="council-card council-live" data-testid="council-progress" role="status" aria-live="polite">
      <div className="council-live-heading"><LoaderCircle size={21} aria-hidden="true" /><div><h3>{councilTitle(council)} is working</h3>
        <p>{latest ? phaseLabel[latest.phase] ?? "The council is working through your question." : "Getting the council ready…"}{latest && latest.activeSubagents > 0 && ` ${latest.activeSubagents} ${latest.activeSubagents === 1 ? "model is" : "models are"} active.`}</p></div></div>
    </section>}
    {progress.length > 0 && <details className="council-details" style={{ margin: "0 0 22px" }}><summary>Discussion activity</summary><ol className="council-activity">{progress.map((frame, index) => <li key={`${frame.occurredAt}-${index}`}>{frame.message}</li>)}</ol></details>}
    {runFailure && <div role="alert" className="council-notice error" data-testid="council-run-failure"><h2>The council could not finish</h2>{runFailure}{runResult && <p>The responses that did finish are saved below.</p>}</div>}
    {runResult && <section data-testid="council-run-result" aria-label="Latest council result"><ResultCard result={runResult} /></section>}
    <div className="councils-toolbar"><h2>Saved results</h2><button type="button" className="council-button quiet" onClick={() => void load()} disabled={status.kind === "loading"} data-testid="council-results-refresh"><RefreshCw size={13} aria-hidden="true" /> Refresh</button></div>
    {status.kind === "loading" && <div data-testid="council-results-loading"><EmptyState state="loading" title="Loading council results" /></div>}
    {status.kind === "unavailable" && <div data-testid="council-results-unavailable"><EmptyState state="unavailable" title="Could not load saved results" description={status.detail} action={<button type="button" className="council-button" onClick={() => void load()}>Try again</button>} /></div>}
    {status.kind === "loaded" && <>
      {status.page.warnings.map((warning, index) => <div key={index} role="status" className="council-notice" data-testid="council-results-warning">{warning}</div>)}
      {status.page.results.length === 0 && status.page.warnings.length === 0 && !runResult && <div data-testid="council-results-empty"><EmptyState title="Your council’s answers will live here" description="Start a council above. Completed answers and partial findings are saved so you can return to them." /></div>}
      {saved.map((result) => <ResultCard key={result.resultId} result={result} />)}
    </>}
  </div></PageShell>;
};

function ResultCard({ result }: { result: CouncilResultCard }) {
  const [open, setOpen] = useState(false);
  const complete = result.status === "completed";
  return <article className="council-card council-result" data-testid={`council-result-${result.resultId}`}>
    <div className="council-card-header"><div className="council-card-heading"><h3>{councilTitle(result.council)}</h3><p>{result.objective}</p></div>
      <span className={`council-status ${complete ? "" : "incomplete"}`} data-testid={`council-result-status-${result.resultId}`}>{complete && <Check size={12} aria-hidden="true" />}{statuses[result.status] ?? "Incomplete"}</span></div>
    <div className="council-result-meta"><span>{relativeTime(result.finishedAt)}</span><span>{repositoryName(result.repository)}</span>{result.evidence && <span>Evidence review</span>}
      {result.costLine && <span data-testid={`council-cost-${result.resultId}`}>{result.costLine}</span>}</div>
    {result.failure && <div role="alert" className="council-notice error" data-testid={`council-failure-${result.resultId}`}>{result.failure}</div>}
    {result.warnings.map((warning, index) => <p key={index} role="status" className="council-note warning">{councilWarning(warning)}</p>)}
    <section className="council-result-answer"><h4><Crown size={14} aria-hidden="true" /> The chair’s answer</h4>
      {result.synthesis ? <div>{renderMarkdown(result.synthesis)}</div> : <p className="council-note" data-testid={`council-no-synthesis-${result.resultId}`}>The chair did not produce a final answer. Any member responses are available below.</p>}
    </section>
    {result.review && <details className="council-member-answer" data-testid={`council-review-${result.resultId}`}>
      <summary><span>Independent review</span><ChevronRight size={13} aria-hidden="true" /></summary>
      <CouncilModel model={result.review.model} role={result.review.role} />
      {result.review.tokens != null && <p className="council-note">{result.review.tokens.toLocaleString()} tokens measured</p>}
      <div className="council-member-body">{renderMarkdown(result.review.response)}</div>
    </details>}
    <button type="button" className="council-button quiet" aria-expanded={open} onClick={() => setOpen((current) => !current)} style={{ marginTop: 12 }} data-testid={`council-toggle-${result.resultId}`}><UsersRound size={14} aria-hidden="true" />{open ? "Hide member responses" : "Explore member responses"}<ChevronRight size={13} aria-hidden="true" /></button>
    {open && <div>{result.rounds.map((round) => <section className="council-round" key={round.round}><h4>Round {round.round} · {round.members.length} {round.members.length === 1 ? "response" : "responses"}</h4>
      {round.members.map((member) => <details className="council-member-answer" key={member.runId}><summary><CouncilModel model={member.model} role={member.role} /><ChevronRight size={13} aria-hidden="true" /></summary>
        {member.tokens != null && <p className="council-note">{member.tokens.toLocaleString()} tokens measured</p>}<div className="council-member-body">{renderMarkdown(member.response)}</div>
      </details>)}
      {round.failures.map((failure, index) => <p key={index} role="status" className="council-note warning">{failure}</p>)}
    </section>)}{result.rounds.length === 0 && <p className="council-note">This saved result has no individual round detail. The full report location is in report details.</p>}</div>}
    <details className="council-details"><summary>Report details</summary><dl>
      <dt>Result ID</dt><dd>{result.resultId}</dd><dt>Council ID</dt><dd>{result.council}</dd><dt>Outcome</dt><dd>{result.status}</dd><dt>Repository</dt><dd>{result.repository}</dd>
      <dt>Started</dt><dd>{result.startedAt}</dd><dt>Finished</dt><dd>{result.finishedAt}</dd><dt>Report</dt><dd>{result.reportMarkdown}</dd>
      {result.originSessionId && <><dt>Original chat</dt><dd>{result.originSessionId}</dd></>}
      {result.participants.length > 0 && <><dt>Recorded participants</dt><dd>{result.participants.map((line, index) => <div key={index}>{line}</div>)}</dd></>}
      {result.warnings.some((warning) => councilWarning(warning) !== warning) && <><dt>Recorded warnings</dt><dd>{result.warnings.map((warning, index) => <div key={index}>{warning}</div>)}</dd></>}
    </dl></details>
  </article>;
}
function councilWarning(warning: string): string {
  const chair = /^chair `([^`]+)` is also a council member; its synthesis will weigh its own report$/.exec(warning);
  return chair ? `${modelLabel(chair[1])} also contributes a member response. As chair, it considers its own findings alongside the other members’ responses.` : warning;
}
function describe(error: unknown) { return error instanceof Error ? error.message : String(error); }
