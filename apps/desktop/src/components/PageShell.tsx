import React from "react";
import { AlertCircle, Inbox, LoaderCircle } from "lucide-react";
import "./pageSurfaces.css";

export const PageShell: React.FC<React.HTMLAttributes<HTMLDivElement>> = ({ className, ...props }) => (
  <div className={["cody-page", className].filter(Boolean).join(" ")} {...props} />
);

export const EmptyState: React.FC<{
  title: string;
  description?: React.ReactNode;
  action?: React.ReactNode;
  state?: "empty" | "loading" | "unavailable";
}> = ({ title, description, action, state = "empty" }) => {
  const Icon = state === "loading" ? LoaderCircle : state === "unavailable" ? AlertCircle : Inbox;
  return (
    <div className={`cody-empty-state ${state}`} role={state === "empty" ? undefined : "status"}>
      <span className="cody-empty-icon"><Icon size={23} strokeWidth={1.5} aria-hidden="true" /></span>
      <h2>{title}</h2>
      {description && <div className="cody-empty-description">{description}</div>}
      {action && <div className="cody-empty-action">{action}</div>}
    </div>
  );
};

export const PageSection: React.FC<{
  title: string;
  description?: string;
  children: React.ReactNode;
  defaultOpen?: boolean;
}> = ({ title, description, children, defaultOpen = false }) => (
  <details className="cody-page-section" open={defaultOpen || undefined}>
    <summary>{title}{description && <span>{description}</span>}</summary>
    <div className="cody-page-section-body">{children}</div>
  </details>
);
