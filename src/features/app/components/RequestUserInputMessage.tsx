import { useCallback, useEffect, useMemo, useState } from "react";
import type {
  RequestUserInputRequest,
  RequestUserInputResponse,
} from "../../../types";

type RequestUserInputMessageProps = {
  requests: RequestUserInputRequest[];
  activeThreadId: string | null;
  activeWorkspaceId?: string | null;
  onSubmit: (
    request: RequestUserInputRequest,
    response: RequestUserInputResponse,
  ) => void;
  onDismiss: (request: RequestUserInputRequest) => void;
};

type SelectionState = Record<string, number | null>;
type NotesState = Record<string, string>;

export function RequestUserInputMessage({
  requests,
  activeThreadId,
  activeWorkspaceId,
  onSubmit,
  onDismiss,
}: RequestUserInputMessageProps) {
  const activeRequests = useMemo(
    () =>
      requests.filter((request) => {
        if (!activeThreadId) {
          return false;
        }
        if (request.params.thread_id !== activeThreadId) {
          return false;
        }
        if (activeWorkspaceId && request.workspace_id !== activeWorkspaceId) {
          return false;
        }
        return true;
      }),
    [requests, activeThreadId, activeWorkspaceId],
  );
  const activeRequest = activeRequests[0] ?? null;
  const questions = useMemo(
    () => activeRequest?.params.questions ?? [],
    [activeRequest],
  );
  const totalRequests = activeRequests.length;

  const [selections, setSelections] = useState<SelectionState>({});
  const [notes, setNotes] = useState<NotesState>({});

  useEffect(() => {
    if (!activeRequest) {
      setSelections({});
      setNotes({});
      return;
    }
    const nextSelections: SelectionState = {};
    const nextNotes: NotesState = {};
    activeRequest.params.questions.forEach((question, index) => {
      const key = question.id || `question-${index}`;
      nextSelections[key] = null;
      nextNotes[key] = "";
    });
    setSelections(nextSelections);
    setNotes(nextNotes);
  }, [activeRequest]);

  const buildAnswers = useCallback(() => {
    const answers: RequestUserInputResponse["answers"] = {};
    questions.forEach((question, index) => {
      if (!question.id) {
        return;
      }
      const answerList: string[] = [];
      const key = question.id || `question-${index}`;
      const selectedIndex = selections[key];
      const options = question.options ?? [];
      const hasOptions = options.length > 0;
      if (hasOptions && selectedIndex !== null) {
        const selected = options[selectedIndex];
        const selectedValue =
          selected?.label?.trim() || selected?.description?.trim() || "";
        if (selectedValue) {
          answerList.push(selectedValue);
        }
      }
      const note = (notes[key] ?? "").trim();
      if (note) {
        if (hasOptions) {
          answerList.push(`user_note: ${note}`);
        } else {
          answerList.push(note);
        }
      }
      answers[question.id] = { answers: answerList };
    });
    return answers;
  }, [questions, selections, notes]);

  const handleSelect = useCallback((questionId: string, optionIndex: number) => {
    setSelections((current) => ({ ...current, [questionId]: optionIndex }));
  }, []);

  const handleNotesChange = useCallback((questionId: string, value: string) => {
    setNotes((current) => ({ ...current, [questionId]: value }));
  }, []);

  const handleSubmit = useCallback(() => {
    if (!activeRequest) return;
    onSubmit(activeRequest, { answers: buildAnswers() });
  }, [activeRequest, onSubmit, buildAnswers]);

  const handleDismiss = useCallback(() => {
    if (!activeRequest) return;
    onDismiss(activeRequest);
  }, [activeRequest, onDismiss]);

  useEffect(() => {
    if (!activeRequest) return;

    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.target instanceof HTMLTextAreaElement) {
        if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
          event.preventDefault();
          handleSubmit();
        }
        return;
      }
      if (event.key === "Enter") {
        event.preventDefault();
        handleSubmit();
      } else if (event.key === "Escape") {
        event.preventDefault();
        handleDismiss();
      }
    };

    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [activeRequest, handleSubmit, handleDismiss]);

  if (!activeRequest) {
    return null;
  }

  return (
    <div className="message request-user-input-message">
      <div
        className="bubble request-user-input-card"
        role="group"
        aria-label="User input requested"
      >
        <div className="request-user-input-header">
          <div className="request-user-input-title">Input requested</div>
          {totalRequests > 1 ? (
            <div className="request-user-input-queue">
              {`Request 1 of ${totalRequests}`}
            </div>
          ) : null}
        </div>
        <div className="request-user-input-body">
          {questions.length ? (
            questions.map((question, index) => {
              const questionId = question.id || `question-${index}`;
              const selectedIndex = selections[questionId];
              const options = question.options ?? [];
              const notePlaceholder = question.isOther
                ? "Type your answer (optional)"
                : options.length
                ? "Add notes (optional)"
                : "Type your answer (optional)";
              return (
                <section key={questionId} className="request-user-input-question">
                  {question.header ? (
                    <div className="request-user-input-question-header">
                      {question.header}
                    </div>
                  ) : null}
                  <div className="request-user-input-question-text">
                    {question.question}
                  </div>
                  {options.length ? (
                    <div className="request-user-input-options">
                      {options.map((option, optionIndex) => (
                        <button
                          key={`${questionId}-${optionIndex}`}
                          type="button"
                          className={`request-user-input-option${
                            selectedIndex === optionIndex ? " is-selected" : ""
                          }`}
                          onClick={() => handleSelect(questionId, optionIndex)}
                        >
                          <div className="request-user-input-option-label">
                            {option.label}
                          </div>
                          {option.description ? (
                            <div className="request-user-input-option-description">
                              {option.description}
                            </div>
                          ) : null}
                        </button>
                      ))}
                    </div>
                  ) : null}
                  <textarea
                    className="request-user-input-notes"
                    placeholder={notePlaceholder}
                    value={notes[questionId] ?? ""}
                    onChange={(event) =>
                      handleNotesChange(questionId, event.target.value)
                    }
                    rows={2}
                  />
                </section>
              );
            })
          ) : (
            <div className="request-user-input-empty">
              No questions provided.
            </div>
          )}
        </div>
        <div className="request-user-input-actions">
          <button className="secondary" onClick={handleDismiss}>
            Dismiss
          </button>
          <button className="primary" onClick={handleSubmit}>
            Submit
          </button>
          <div className="request-user-input-shortcuts">
            <span><kbd>Enter</kbd> Submit</span>
            <span><kbd>Esc</kbd> Dismiss</span>
          </div>
        </div>
      </div>
    </div>
  );
}
