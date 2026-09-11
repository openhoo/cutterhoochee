import { useEffect, useRef, useState, type FormEvent } from "react";
import {
  ArrowLeft,
  Check,
  ChevronRight,
  CircleHelp,
  Download,
  Film,
  FolderOpen,
  Moon,
  Plus,
  Settings2,
  Sun,
} from "lucide-react";

import type { EditorClient, ProjectStatus } from "@cutterhoochee/shared";
import { Button } from "@/components/ui/button";
import { callNative, isEditorClientError } from "@/lib/native";
import "./project-start.css";

type Theme = "dark" | "light";
type Connection = "connected" | "unavailable";
type Step = "welcome" | "format" | "details";
type BusyAction = "create" | "open" | "recover" | null;
type FeedbackTone = "error" | "notice";

type Feedback = {
  tone: FeedbackTone;
  message: string;
};

type ProjectPreset = {
  id: "vertical" | "landscape" | "square";
  label: string;
  aspect: "9:16" | "16:9" | "1:1";
  width: number;
  height: number;
  use: string;
};

const PROJECT_PRESETS: readonly ProjectPreset[] = [
  {
    id: "vertical",
    label: "Vertical video",
    aspect: "9:16",
    width: 1080,
    height: 1920,
    use: "Shorts · Reels · TikTok",
  },
  {
    id: "landscape",
    label: "Landscape",
    aspect: "16:9",
    width: 1920,
    height: 1080,
    use: "YouTube · film",
  },
  {
    id: "square",
    label: "Square",
    aspect: "1:1",
    width: 1080,
    height: 1080,
    use: "Social posts",
  },
];

const FRAME_RATES = [24, 25, 30, 60] as const;
const DEFAULT_PRESET: ProjectPreset["id"] = "landscape";
const DEFAULT_PROJECT_NAME = "Untitled project";
const BROWSER_LIMITATION =
  "The browser preview cannot open native folder dialogs. Use the Cutterhoochee desktop app to create or open a project.";

export type ProjectStartScreenProps = {
  client: EditorClient;
  status: ProjectStatus;
  connection: Connection;
  theme: Theme;
  onThemeChange: (theme: Theme) => void;
  onProjectReady: (status: ProjectStatus) => Promise<void>;
};

function formatPixels(preset: ProjectPreset): string {
  return `${preset.width} × ${preset.height}`;
}

function errorMessage(error: unknown): string {
  if (isEditorClientError(error)) return `${error.code}: ${error.message}`;
  if (error instanceof Error && error.message.trim().length > 0) return error.message;
  return "The desktop operation could not be completed.";
}

function isCancellation(error: unknown): boolean {
  const message = isEditorClientError(error)
    ? error.message
    : error instanceof Error
      ? error.message
      : typeof error === "string"
        ? error
        : "";
  return /cancel(l)?ed|cancellation/i.test(message);
}

function cancellationMessage(action: "create" | "open"): string {
  return action === "create"
    ? "Folder selection cancelled. Your format, frame rate, and project name are still here."
    : "No project was opened. You can try again whenever you are ready.";
}

function nativeUnavailableMessage(): Feedback {
  return { tone: "notice", message: BROWSER_LIMITATION };
}

export function ProjectStartScreen({
  client,
  status,
  connection,
  theme,
  onThemeChange,
  onProjectReady,
}: ProjectStartScreenProps) {
  const [step, setStep] = useState<Step>("welcome");
  const [presetId, setPresetId] = useState<ProjectPreset["id"]>(DEFAULT_PRESET);
  const [name, setName] = useState(DEFAULT_PROJECT_NAME);
  const [fps, setFps] = useState<(typeof FRAME_RATES)[number]>(30);
  const [advancedOpen, setAdvancedOpen] = useState(false);
  const [busy, setBusy] = useState<BusyAction>(null);
  const [feedback, setFeedback] = useState<Feedback | null>(null);
  const busyRef = useRef<BusyAction>(null);
  const stepHeadingRef = useRef<HTMLHeadingElement>(null);
  const nameInputRef = useRef<HTMLInputElement>(null);
  const nativeAvailable = connection === "connected";
  const preset = PROJECT_PRESETS.find((candidate) => candidate.id === presetId) ?? PROJECT_PRESETS[1];

  useEffect(() => {
    stepHeadingRef.current?.focus();
  }, [step]);

  useEffect(() => {
    if (!status.open || !nativeAvailable || busyRef.current) return;

    busyRef.current = "recover";
    setBusy("recover");
    setFeedback(null);
    let active = true;
    void (async () => {
      try {
        const currentStatus = await client.projectStatus();
        if (active && currentStatus.open) await onProjectReady(currentStatus);
      } catch (caught) {
        if (active) setFeedback({ tone: "error", message: errorMessage(caught) });
      } finally {
        if (active) {
          busyRef.current = null;
          setBusy(null);
        }
      }
    })();
    return () => {
      active = false;
      if (busyRef.current === "recover") {
        busyRef.current = null;
        setBusy(null);
      }
    };
  }, [client, nativeAvailable, onProjectReady, status.open]);

  const beginBusy = (action: Exclude<BusyAction, null>): boolean => {
    if (busyRef.current) return false;
    busyRef.current = action;
    setBusy(action);
    setFeedback(null);
    return true;
  };

  const finishBusy = () => {
    busyRef.current = null;
    setBusy(null);
  };

  const readOpenStatus = async (): Promise<ProjectStatus | null> => {
    try {
      const currentStatus = await client.projectStatus();
      return currentStatus.open ? currentStatus : null;
    } catch {
      return null;
    }
  };

  const handleOpenProject = async () => {
    if (!nativeAvailable) {
      setFeedback(nativeUnavailableMessage());
      return;
    }
    if (!beginBusy("open")) return;

    let readyAttempted = false;
    try {
      const currentStatus = await client.projectStatus();
      if (currentStatus.open) {
        readyAttempted = true;
        await onProjectReady(currentStatus);
        return;
      }

      await callNative(client, { method: "project_open", params: {} });
      const nextStatus = await client.projectStatus();
      if (!nextStatus.open) throw new Error("The desktop app did not report an open project.");
      readyAttempted = true;
      await onProjectReady(nextStatus);
    } catch (caught) {
      if (isCancellation(caught)) {
        setFeedback({ tone: "notice", message: cancellationMessage("open") });
      } else if (!readyAttempted) {
        setFeedback({ tone: "error", message: errorMessage(caught) });
      } else {
        // The project is already open; do not repeat the native operation after a load error.
        setFeedback({ tone: "error", message: errorMessage(caught) });
      }
    } finally {
      finishBusy();
    }
  };

  const handleCreateProject = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (busyRef.current) return;

    const trimmedName = name.trim();
    if (trimmedName.length === 0) {
      setFeedback({ tone: "error", message: "Enter a project name before continuing." });
      nameInputRef.current?.focus();
      return;
    }
    if (!nativeAvailable) {
      setFeedback(nativeUnavailableMessage());
      return;
    }
    if (!beginBusy("create")) return;

    let createRequested = false;
    let readyAttempted = false;
    try {
      // A previous native request may have completed while this screen was interrupted.
      // Read the authoritative status first so pressing Create cannot make a duplicate.
      const currentStatus = await client.projectStatus();
      if (currentStatus.open) {
        readyAttempted = true;
        await onProjectReady(currentStatus);
        return;
      }

      createRequested = true;
      await callNative(client, {
        method: "project_create",
        params: {
          name: trimmedName,
          aspect: preset.aspect,
          fpsNum: fps,
          fpsDen: 1,
        },
      });
      const nextStatus = await client.projectStatus();
      if (!nextStatus.open) throw new Error("The desktop app did not report the new project as open.");
      readyAttempted = true;
      await onProjectReady(nextStatus);
    } catch (caught) {
      if (isCancellation(caught)) {
        setFeedback({ tone: "notice", message: cancellationMessage("create") });
      } else if (createRequested && !readyAttempted) {
        // A native create can finish even if its reply or the follow-up status read is
        // interrupted. Recover by reading status, never by issuing project_create again.
        const recoveredStatus = await readOpenStatus();
        if (recoveredStatus) {
          try {
            readyAttempted = true;
            await onProjectReady(recoveredStatus);
            return;
          } catch (recoveryError) {
            setFeedback({ tone: "error", message: errorMessage(recoveryError) });
          }
        } else {
          setFeedback({ tone: "error", message: errorMessage(caught) });
        }
      } else {
        // In particular, do not retry project_create when onProjectReady rejects.
        setFeedback({ tone: "error", message: errorMessage(caught) });
      }
    } finally {
      finishBusy();
    }
  };

  const handleStartProject = () => {
    if (!nativeAvailable) {
      setFeedback(nativeUnavailableMessage());
      return;
    }
    setFeedback(null);
    setStep("format");
  };

  const handleBackToWelcome = () => {
    if (busyRef.current) return;
    setFeedback(null);
    setStep("welcome");
  };

  const handleFormatSubmit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (busyRef.current) return;
    setFeedback(null);
    setStep("details");
  };

  const feedbackNode = feedback ? (
    <div
      className={`project-start-feedback project-start-feedback--${feedback.tone}`}
      role={feedback.tone === "error" ? "alert" : "status"}
      aria-live="polite"
    >
      {feedback.tone === "error" ? <CircleHelp aria-hidden="true" /> : <Check aria-hidden="true" />}
      <span>{feedback.message}</span>
    </div>
  ) : null;

  const themeLabel = theme === "dark" ? "Switch to light theme" : "Switch to dark theme";

  return (
    <div className={`project-start-shell project-start-shell--${theme}`}>
      <header className="project-start-topbar">
        <div className="project-start-brand-lockup">
          <span className="project-start-brand-mark"><Film aria-hidden="true" /></span>
          <span className="project-start-brand-name">Cutterhoochee</span>
        </div>
        <div className="project-start-topbar-actions">
          <span className="project-start-connection" aria-live="polite">
            <span className={`project-start-status-dot${nativeAvailable ? "" : " project-start-status-dot--muted"}`} />
            {nativeAvailable ? "Desktop ready" : "Browser preview · desktop required"}
          </span>
          <button
            className="project-start-theme-toggle"
            type="button"
            aria-label={themeLabel}
            title={themeLabel}
            onClick={() => onThemeChange(theme === "dark" ? "light" : "dark")}
          >
            {theme === "dark" ? <Sun aria-hidden="true" /> : <Moon aria-hidden="true" />}
          </button>
        </div>
      </header>

      {step === "welcome" ? (
        <main className="project-start-content">
          <section className="project-start-intro" aria-labelledby="project-start-welcome-title">
            <p className="project-start-eyebrow">Your next story</p>
            <h1 id="project-start-welcome-title" ref={stepHeadingRef} tabIndex={-1}>A little space to create.</h1>
            <p>Start something new, or pick up where you left off.</p>
          </section>

          <div className="project-start-welcome-grid">
            <section className="project-start-card project-start-card--primary" aria-labelledby="project-start-new-title">
              <div className="project-start-card-heading">
                <span className="project-start-card-icon"><Plus aria-hidden="true" /></span>
                <div>
                  <p className="project-start-card-kicker">Create</p>
                  <h2 id="project-start-new-title">New project</h2>
                </div>
              </div>
              <p className="project-start-card-copy">Choose a format, give it a name, and you're ready to edit.</p>
              <div className="project-start-card-detail">
                <span className="project-start-detail-mark" aria-hidden="true"><Check /></span>
                <span>Made for everything from short videos to films</span>
              </div>
              <Button
                className="project-start-primary-action"
                type="button"
                variant="primary"
                size="md"
                onClick={handleStartProject}
                disabled={busy !== null}
              >
                <Plus aria-hidden="true" />
                {busy === "recover" ? "Reconnecting…" : "Start a new project"}
              </Button>
            </section>

            <section className="project-start-card" aria-labelledby="project-start-open-title">
              <div className="project-start-card-heading">
                <span className="project-start-card-icon project-start-card-icon--muted"><FolderOpen aria-hidden="true" /></span>
                <div>
                  <p className="project-start-card-kicker">Continue</p>
                  <h2 id="project-start-open-title">Open project</h2>
                </div>
              </div>
              <p className="project-start-card-copy">Choose a saved <code>.cutproj</code> folder and continue your story.</p>
              <Button
                className="project-start-secondary-action"
                type="button"
                variant="secondary"
                size="md"
                onClick={() => void handleOpenProject()}
                disabled={busy !== null}
              >
                <FolderOpen aria-hidden="true" />
                {busy === "open" ? "Opening…" : "Open project"}
              </Button>
              <p className="project-start-card-note">Your project and its media stay together on your device.</p>
            </section>
          </div>

          <section className="project-start-native-note" aria-labelledby="project-start-native-note-title">
            <Download aria-hidden="true" />
            <div>
              <h2 id="project-start-native-note-title">Or start with your footage</h2>
              <p>Drop videos, photos, or audio onto this window. Choose where to save, and we'll start a landscape project. Want a vertical video? Create a project first.</p>
            </div>
          </section>

          {!nativeAvailable ? (
            <aside className="project-start-browser-note" aria-label="Desktop app limitation">
              <FolderOpen aria-hidden="true" />
              <div>
                <strong>Desktop app required for projects</strong>
                <p>{BROWSER_LIMITATION} Dragging a browser file here will not import it.</p>
              </div>
            </aside>
          ) : null}

          {feedbackNode}
        </main>
      ) : (
        <main className="project-start-wizard-main">
          <nav className="project-start-wizard-nav" aria-label="Project setup">
            <button className="project-start-back-button" type="button" onClick={handleBackToWelcome} disabled={busy !== null}>
              <ArrowLeft aria-hidden="true" />
              <span>Back to welcome</span>
            </button>
            <ol className="project-start-step-list">
              <li className={step === "format" ? "project-start-step project-start-step--active" : "project-start-step"} aria-current={step === "format" ? "step" : undefined}>
                <span>1</span>
                <strong>Format</strong>
              </li>
              <li className={step === "details" ? "project-start-step project-start-step--active" : "project-start-step"} aria-current={step === "details" ? "step" : undefined}>
                <span>2</span>
                <strong>Name &amp; review</strong>
              </li>
            </ol>
          </nav>

          <section className="project-start-wizard-panel">
            {step === "format" ? (
              <form key="format" onSubmit={handleFormatSubmit}>
                <div className="project-start-step-heading">
                  <p className="project-start-eyebrow">Step 1 of 2</p>
                  <h1 ref={stepHeadingRef} tabIndex={-1}>Choose a visual format</h1>
                  <p>Where will your story live?</p>
                </div>

                <fieldset className="project-start-format-fieldset">
                  <legend className="project-start-sr-only">Project format preset</legend>
                  <div className="project-start-format-grid">
                    {PROJECT_PRESETS.map((candidate) => {
                      const selected = candidate.id === presetId;
                      return (
                        <label className={`project-start-format-choice${selected ? " project-start-format-choice--selected" : ""}`} key={candidate.id}>
                          <input
                            className="project-start-choice-input project-start-sr-only"
                            type="radio"
                            name="project-format"
                            value={candidate.id}
                            checked={selected}
                            onChange={() => setPresetId(candidate.id)}
                          />
                          <span className="project-start-format-choice-body">
                            <span className={`project-start-format-visual project-start-format-visual--${candidate.id}`} aria-hidden="true"><span /></span>
                            <span className="project-start-format-choice-copy">
                              <strong>{candidate.label}</strong>
                              <span className="project-start-format-ratio">{candidate.aspect}</span>
                              <span className="project-start-format-dimensions">{formatPixels(candidate)}</span>
                              <span className="project-start-format-use">{candidate.use}</span>
                            </span>
                            <span className="project-start-format-check" aria-hidden="true"><Check /></span>
                          </span>
                        </label>
                      );
                    })}
                  </div>
                </fieldset>

                <div className="project-start-form-actions">
                  <Button type="submit" variant="primary" size="md">
                    Continue to details
                    <ChevronRight aria-hidden="true" />
                  </Button>
                </div>
                {feedbackNode}
              </form>
            ) : (
              <form key="details" onSubmit={handleCreateProject}>
                <div className="project-start-step-heading">
                  <p className="project-start-eyebrow">Step 2 of 2</p>
                  <h1 ref={stepHeadingRef} tabIndex={-1}>Name and review your project</h1>
                  <p>Give your story a name. We'll ask where to save it next.</p>
                </div>

                <div className="project-start-details-grid">
                  <div className="project-start-details-column">
                    <label className="project-start-field" htmlFor="project-start-name">
                      <span className="project-start-field-label">Project name</span>
                      <input
                        ref={nameInputRef}
                        id="project-start-name"
                        name="projectName"
                        type="text"
                        value={name}
                        onChange={(event) => {
                          setName(event.target.value);
                          if (feedback?.tone === "error") setFeedback(null);
                        }}
                        placeholder={DEFAULT_PROJECT_NAME}
                        autoComplete="off"
                        maxLength={80}
                        required
                        aria-describedby="project-start-name-help"
                      />
                      <span className="project-start-field-help" id="project-start-name-help">Choose a name that's easy to find later.</span>
                    </label>

                    <div className="project-start-advanced">
                      <button
                        className="project-start-advanced-toggle"
                        type="button"
                        aria-expanded={advancedOpen}
                        aria-controls="project-start-advanced-settings"
                        onClick={() => setAdvancedOpen((open) => !open)}
                      >
                        <ChevronRight className={advancedOpen ? "project-start-chevron project-start-chevron--open" : "project-start-chevron"} aria-hidden="true" />
                        <Settings2 aria-hidden="true" />
                        <span>Advanced settings</span>
                      </button>
                      {advancedOpen ? (
                        <div className="project-start-advanced-settings" id="project-start-advanced-settings">
                          <label className="project-start-field" htmlFor="project-start-fps">
                            <span className="project-start-field-label">Frame rate</span>
                            <select
                              id="project-start-fps"
                              name="frameRate"
                              value={fps}
                              onChange={(event) => setFps(Number(event.target.value) as (typeof FRAME_RATES)[number])}
                              aria-describedby="project-start-fps-help"
                            >
                              {FRAME_RATES.map((rate) => <option value={rate} key={rate}>{rate} fps</option>)}
                            </select>
                            <span className="project-start-field-help" id="project-start-fps-help">30 fps is a good choice for most videos.</span>
                          </label>
                        </div>
                      ) : null}
                    </div>
                  </div>

                  <div className="project-start-details-column">
                    <section className="project-start-review" aria-labelledby="project-start-review-title">
                      <div className="project-start-review-heading">
                        <span className="project-start-review-icon"><Check aria-hidden="true" /></span>
                        <div>
                          <p className="project-start-eyebrow">Review</p>
                          <h2 id="project-start-review-title">{preset.label} project</h2>
                        </div>
                      </div>
                      <dl className="project-start-review-list">
                        <div><dt>Format</dt><dd>{preset.aspect}</dd></div>
                        <div><dt>Dimensions</dt><dd>{formatPixels(preset)}</dd></div>
                        <div><dt>Frame rate</dt><dd>{fps} fps</dd></div>
                      </dl>
                    </section>

                    <aside className="project-start-save-note">
                      <FolderOpen aria-hidden="true" />
                      <div>
                        <strong>A home for your project</strong>
                        <p>We'll create <code>{trimmedProjectFolderName(name)}.cutproj</code> in the folder you choose.</p>
                      </div>
                    </aside>
                  </div>
                </div>

                <div className="project-start-form-actions project-start-form-actions--details">
                  <Button type="button" variant="ghost" size="md" onClick={() => { setFeedback(null); setStep("format"); }} disabled={busy !== null}>
                    <ArrowLeft aria-hidden="true" />
                    Back to format
                  </Button>
                  <Button type="submit" variant="primary" size="md" disabled={busy !== null || !nativeAvailable}>
                    {busy === "create" ? <span className="project-start-button-spinner" aria-hidden="true" /> : <Plus aria-hidden="true" />}
                    {busy === "create" ? "Creating…" : "Create project"}
                  </Button>
                </div>
                {!nativeAvailable ? <p className="project-start-inline-limit" role="status">{BROWSER_LIMITATION}</p> : null}
                {feedbackNode}
              </form>
            )}
          </section>
        </main>
      )}

      <footer className="project-start-footer">
        <span>Your files stay on your device</span>
        <span>Make room for your story.</span>
      </footer>
    </div>
  );
}

function trimmedProjectFolderName(value: string): string {
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : DEFAULT_PROJECT_NAME;
}
