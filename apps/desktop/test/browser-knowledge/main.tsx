import { createRoot } from "react-dom/client";
import { App } from "../../src/App.js";
import "../../src/theme.css";
import { KnowledgeSessionFixture } from "../fixtures/knowledgeSession.js";

const fixture = new KnowledgeSessionFixture();
createRoot(document.getElementById("root")!).render(
  <App initialView="sessions" makeTransport={() => fixture.transport} knowledge={fixture.knowledge} />,
);
