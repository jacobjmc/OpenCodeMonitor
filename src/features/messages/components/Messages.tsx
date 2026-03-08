import {
  memo,
  type ReactNode,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import ChevronDown from "lucide-react/dist/esm/icons/chevron-down";
import ChevronUp from "lucide-react/dist/esm/icons/chevron-up";
import type {
  ConversationItem,
  OpenAppTarget,
  RequestUserInputRequest,
  RequestUserInputResponse,
} from "../../../types";
import { isPlanReadyTaggedMessage } from "../../../utils/internalPlanReadyMessages";
import { PlanReadyFollowupMessage } from "../../app/components/PlanReadyFollowupMessage";
import { RequestUserInputMessage } from "../../app/components/RequestUserInputMessage";
import { useFileLinkOpener } from "../hooks/useFileLinkOpener";
import {
  SCROLL_THRESHOLD_PX,
  buildToolGroups,
  formatCount,
  parseReasoning,
  scrollKeyForItems,
  toolStatusTone,
} from "../utils/messageRenderUtils";
import {
  DiffRow,
  ExploreRow,
  MessageRow,
  ReasoningRow,
  ReviewRow,
  TodoRow,
  ToolRow,
  WorkingIndicator,
} from "./MessageRows";

type MessagesProps = {
  items: ConversationItem[];
  threadId: string | null;
  workspaceId?: string | null;
  isThinking: boolean;
  isLoadingMessages?: boolean;
  processingStartedAt?: number | null;
  lastDurationMs?: number | null;
  workspacePath?: string | null;
  openTargets: OpenAppTarget[];
  selectedOpenAppId: string;
  codeBlockCopyUseModifier?: boolean;
  showMessageFilePath?: boolean;
  userInputRequests?: RequestUserInputRequest[];
  onUserInputSubmit?: (
    request: RequestUserInputRequest,
    response: RequestUserInputResponse,
  ) => void;
  onUserInputDismiss?: (request: RequestUserInputRequest) => void;
  onPlanAccept?: () => void;
  onPlanSubmitChanges?: (changes: string) => void;
  onOpenThreadLink?: (threadId: string) => void;
  onQuoteMessage?: (text: string) => void;
};

type MessageListRow = {
  id: string;
  node: ReactNode;
};

const SINGLE_FENCED_BLOCK_PATTERN =
  /^\s*(```|~~~)[^\r\n]*\r?\n([\s\S]*?)\r?\n\1\s*$/;
const VIRTUALIZATION_ROW_THRESHOLD = 30;

function getCopyableMessageText(text: string) {
  const fencedMatch = text.match(SINGLE_FENCED_BLOCK_PATTERN);
  if (!fencedMatch) {
    return text;
  }
  return fencedMatch[2];
}

export const Messages = memo(function Messages({
  items,
  threadId,
  workspaceId = null,
  isThinking,
  isLoadingMessages = false,
  processingStartedAt = null,
  lastDurationMs = null,
  workspacePath = null,
  openTargets,
  selectedOpenAppId,
  codeBlockCopyUseModifier = false,
  showMessageFilePath = true,
  userInputRequests = [],
  onUserInputSubmit,
  onUserInputDismiss,
  onPlanAccept,
  onPlanSubmitChanges,
  onOpenThreadLink,
  onQuoteMessage,
}: MessagesProps) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const autoScrollRef = useRef(true);
  const [expandedItems, setExpandedItems] = useState<Set<string>>(new Set());
  const manuallyToggledExpandedRef = useRef<Set<string>>(new Set());
  const [collapsedToolGroups, setCollapsedToolGroups] = useState<Set<string>>(
    new Set(),
  );
  const [copiedMessageId, setCopiedMessageId] = useState<string | null>(null);
  const copyTimeoutRef = useRef<number | null>(null);
  const activeUserInputRequestId =
    threadId && userInputRequests.length
      ? (userInputRequests.find(
          (request) =>
            request.params.thread_id === threadId &&
            (!workspaceId || request.workspace_id === workspaceId),
        )?.request_id ?? null)
      : null;
  const scrollKey = `${scrollKeyForItems(items)}-${activeUserInputRequestId ?? "no-input"}`;
  const { openFileLink, showFileLinkMenu } = useFileLinkOpener(
    workspacePath,
    openTargets,
    selectedOpenAppId,
  );

  const isNearBottom = useCallback(
    (node: HTMLDivElement) =>
      node.scrollHeight - node.scrollTop - node.clientHeight <= SCROLL_THRESHOLD_PX,
    [],
  );

  const updateAutoScroll = () => {
    if (!containerRef.current) {
      return;
    }
    autoScrollRef.current = isNearBottom(containerRef.current);
  };

  const requestAutoScroll = useCallback(() => {
    const container = containerRef.current;
    const shouldScroll =
      autoScrollRef.current || (container ? isNearBottom(container) : true);
    if (!shouldScroll) {
      return;
    }
    if (container) {
      container.scrollTop = container.scrollHeight;
      return;
    }
  }, [isNearBottom]);

  useLayoutEffect(() => {
    autoScrollRef.current = true;
  }, [threadId]);

  const toggleExpanded = useCallback((id: string) => {
    manuallyToggledExpandedRef.current.add(id);
    setExpandedItems((prev) => {
      const next = new Set(prev);
      if (next.has(id)) {
        next.delete(id);
      } else {
        next.add(id);
      }
      return next;
    });
  }, []);

  const toggleToolGroup = useCallback((id: string) => {
    setCollapsedToolGroups((prev) => {
      const next = new Set(prev);
      if (next.has(id)) {
        next.delete(id);
      } else {
        next.add(id);
      }
      return next;
    });
  }, []);

  const reasoningMetaById = useMemo(() => {
    const meta = new Map<string, ReturnType<typeof parseReasoning>>();
    items.forEach((item) => {
      if (item.kind === "reasoning") {
        meta.set(item.id, parseReasoning(item));
      }
    });
    return meta;
  }, [items]);

  const latestReasoningLabel = useMemo(() => {
    for (let index = items.length - 1; index >= 0; index -= 1) {
      const item = items[index];
      if (item.kind === "message") {
        break;
      }
      if (item.kind !== "reasoning") {
        continue;
      }
      const parsed = reasoningMetaById.get(item.id);
      if (parsed?.workingLabel) {
        return parsed.workingLabel;
      }
    }
    return null;
  }, [items, reasoningMetaById]);

  const visibleItems = useMemo(
    () =>
      items.filter((item) => {
        if (
          item.kind === "message" &&
          item.role === "user" &&
          isPlanReadyTaggedMessage(item.text)
        ) {
          return false;
        }
        if (item.kind !== "reasoning") {
          return true;
        }
        return Boolean(reasoningMetaById.get(item.id)?.workingLabel);
      }),
    [items, reasoningMetaById],
  );

  const hasVisibleReasoningInActiveTail = useMemo(() => {
    for (let index = visibleItems.length - 1; index >= 0; index -= 1) {
      const item = visibleItems[index];
      if (item.kind === "message") {
        break;
      }
      if (item.kind !== "reasoning") {
        continue;
      }
      if (reasoningMetaById.get(item.id)?.workingLabel) {
        return true;
      }
    }
    return false;
  }, [visibleItems, reasoningMetaById]);

  const workingReasoningLabel =
    isThinking && hasVisibleReasoningInActiveTail ? null : latestReasoningLabel;

  useEffect(() => {
    const itemsToExpand: string[] = [];
    for (let index = visibleItems.length - 1; index >= 0; index -= 1) {
      const item = visibleItems[index];
      if (manuallyToggledExpandedRef.current.has(item.id)) {
        continue;
      }
      if (
        item.kind === "tool" &&
        item.toolType === "plan" &&
        (item.output ?? "").trim().length > 0
      ) {
        itemsToExpand.push(item.id);
        break;
      }
      if (
        item.kind === "tool" &&
        item.toolType === "fileChange" &&
        item.changes?.some((change) => change.diff)
      ) {
        itemsToExpand.push(item.id);
      }
    }
    if (itemsToExpand.length === 0) {
      return;
    }
    setExpandedItems((prev) => {
      const next = new Set(prev);
      let changed = false;
      for (const id of itemsToExpand) {
        if (!next.has(id)) {
          next.add(id);
          changed = true;
        }
      }
      return changed ? next : prev;
    });
  }, [visibleItems]);

  useEffect(() => {
    return () => {
      if (copyTimeoutRef.current) {
        window.clearTimeout(copyTimeoutRef.current);
      }
    };
  }, []);

  const handleCopyMessage = useCallback(
    async (item: Extract<ConversationItem, { kind: "message" }>) => {
      try {
        await navigator.clipboard.writeText(getCopyableMessageText(item.text));
        setCopiedMessageId(item.id);
        if (copyTimeoutRef.current) {
          window.clearTimeout(copyTimeoutRef.current);
        }
        copyTimeoutRef.current = window.setTimeout(() => {
          setCopiedMessageId(null);
        }, 1200);
      } catch {
        // No-op: clipboard errors can occur in restricted contexts.
      }
    },
    [],
  );

  const toMarkdownQuote = useCallback((text: string): string => {
    return text
      .split("\n")
      .map((line) => `> ${line}`)
      .join("\n");
  }, []);

  const handleQuoteMessage = useCallback(
    (item: Extract<ConversationItem, { kind: "message" }>) => {
      if (!onQuoteMessage) return;
      const quoted = toMarkdownQuote(item.text);
      onQuoteMessage(quoted + "\n\n");
    },
    [onQuoteMessage, toMarkdownQuote],
  );

  useLayoutEffect(() => {
    const container = containerRef.current;
    const shouldScroll =
      autoScrollRef.current ||
      (container ? isNearBottom(container) : true);
    if (!shouldScroll) {
      return;
    }
    if (container) {
      container.scrollTop = container.scrollHeight;
      return;
    }
  }, [scrollKey, isThinking, isNearBottom, threadId]);

  const groupedItems = useMemo(() => buildToolGroups(visibleItems), [visibleItems]);

  const hasActiveUserInputRequest = activeUserInputRequestId !== null;
  const hasVisibleUserInputRequest =
    hasActiveUserInputRequest && Boolean(onUserInputSubmit) && Boolean(onUserInputDismiss);
  const userInputNode =
    hasActiveUserInputRequest && onUserInputSubmit && onUserInputDismiss ? (
      <RequestUserInputMessage
        requests={userInputRequests}
        activeThreadId={threadId}
        activeWorkspaceId={workspaceId}
        onSubmit={onUserInputSubmit}
        onDismiss={onUserInputDismiss}
      />
    ) : null;

  const [dismissedPlanFollowupByThread, setDismissedPlanFollowupByThread] =
    useState<Record<string, string>>({});

  const planFollowup = useMemo(() => {
    if (!threadId) {
      return { shouldShow: false, planItemId: null as string | null };
    }
    if (!onPlanAccept || !onPlanSubmitChanges) {
      return { shouldShow: false, planItemId: null as string | null };
    }
    if (hasVisibleUserInputRequest) {
      return { shouldShow: false, planItemId: null as string | null };
    }
    let planIndex = -1;
    let planItem: Extract<ConversationItem, { kind: "tool" }> | null = null;
    for (let index = items.length - 1; index >= 0; index -= 1) {
      const item = items[index];
      if (item.kind === "tool" && item.toolType === "plan") {
        planIndex = index;
        planItem = item;
        break;
      }
    }
    if (!planItem) {
      return { shouldShow: false, planItemId: null as string | null };
    }
    const planItemId = planItem.id;
    if (dismissedPlanFollowupByThread[threadId] === planItemId) {
      return { shouldShow: false, planItemId };
    }
    if (!(planItem.output ?? "").trim()) {
      return { shouldShow: false, planItemId };
    }
    const planTone = toolStatusTone(planItem, false);
    if (planTone === "failed") {
      return { shouldShow: false, planItemId };
    }
    // Some backends stream plan output deltas without a final status update. As
    // soon as the turn stops thinking, treat the latest plan output as ready.
    if (isThinking && planTone !== "completed") {
      return { shouldShow: false, planItemId };
    }
    for (let index = planIndex + 1; index < items.length; index += 1) {
      const item = items[index];
      if (item.kind === "message" && item.role === "user") {
        return { shouldShow: false, planItemId };
      }
    }
    return { shouldShow: true, planItemId };
  }, [
    dismissedPlanFollowupByThread,
    hasVisibleUserInputRequest,
    isThinking,
    items,
    onPlanAccept,
    onPlanSubmitChanges,
    threadId,
  ]);

  const planFollowupNode =
    planFollowup.shouldShow && onPlanAccept && onPlanSubmitChanges ? (
      <PlanReadyFollowupMessage
        onAccept={() => {
          if (threadId && planFollowup.planItemId) {
            setDismissedPlanFollowupByThread((prev) => ({
              ...prev,
              [threadId]: planFollowup.planItemId!,
            }));
          }
          onPlanAccept();
        }}
        onSubmitChanges={(changes) => {
          if (threadId && planFollowup.planItemId) {
            setDismissedPlanFollowupByThread((prev) => ({
              ...prev,
              [threadId]: planFollowup.planItemId!,
            }));
          }
          onPlanSubmitChanges(changes);
        }}
      />
    ) : null;

  const renderItem = (item: ConversationItem) => {
    if (item.kind === "message") {
      const isCopied = copiedMessageId === item.id;
      return (
        <MessageRow
          key={item.id}
          item={item}
          isCopied={isCopied}
          onCopy={handleCopyMessage}
          onQuote={item.role === "assistant" ? handleQuoteMessage : undefined}
          codeBlockCopyUseModifier={codeBlockCopyUseModifier}
          showMessageFilePath={showMessageFilePath}
          workspacePath={workspacePath}
          onOpenFileLink={openFileLink}
          onOpenFileLinkMenu={showFileLinkMenu}
          onOpenThreadLink={onOpenThreadLink}
        />
      );
    }
    if (item.kind === "reasoning") {
      const isExpanded = expandedItems.has(item.id);
      const parsed = reasoningMetaById.get(item.id) ?? parseReasoning(item);
      return (
        <ReasoningRow
          key={item.id}
          item={item}
          parsed={parsed}
          isExpanded={isExpanded}
          onToggle={toggleExpanded}
          showMessageFilePath={showMessageFilePath}
          workspacePath={workspacePath}
          onOpenFileLink={openFileLink}
          onOpenFileLinkMenu={showFileLinkMenu}
          onOpenThreadLink={onOpenThreadLink}
        />
      );
    }
    if (item.kind === "review") {
      return (
        <ReviewRow
          key={item.id}
          item={item}
          showMessageFilePath={showMessageFilePath}
          workspacePath={workspacePath}
          onOpenFileLink={openFileLink}
          onOpenFileLinkMenu={showFileLinkMenu}
          onOpenThreadLink={onOpenThreadLink}
        />
      );
    }
    if (item.kind === "diff") {
      return <DiffRow key={item.id} item={item} />;
    }
    if (item.kind === "tool") {
      const isExpanded = expandedItems.has(item.id);
      return (
        <ToolRow
          key={item.id}
          item={item}
          isExpanded={isExpanded}
          onToggle={toggleExpanded}
          showMessageFilePath={showMessageFilePath}
          workspacePath={workspacePath}
          onOpenFileLink={openFileLink}
          onOpenFileLinkMenu={showFileLinkMenu}
          onOpenThreadLink={onOpenThreadLink}
          onRequestAutoScroll={requestAutoScroll}
        />
      );
    }
    if (item.kind === "explore") {
      return <ExploreRow key={item.id} item={item} />;
    }
    if (item.kind === "todo") {
      return <TodoRow key={item.id} item={item} />;
    }
    return null;
  };

  const rows = useMemo<MessageListRow[]>(() => {
    const nextRows: MessageListRow[] = [];
    groupedItems.forEach((entry) => {
      if (entry.kind === "toolGroup") {
        const { group } = entry;
        const isCollapsed = collapsedToolGroups.has(group.id);
        const summaryParts = [
          formatCount(group.toolCount, "tool call", "tool calls"),
        ];
        if (group.messageCount > 0) {
          summaryParts.push(formatCount(group.messageCount, "message", "messages"));
        }
        const summaryText = summaryParts.join(", ");
        const groupBodyId = `tool-group-${group.id}`;
        const ChevronIcon = isCollapsed ? ChevronDown : ChevronUp;
        nextRows.push({
          id: `tool-group-${group.id}`,
          node: (
            <div className={`tool-group ${isCollapsed ? "tool-group-collapsed" : ""}`}>
              <div className="tool-group-header">
                <button
                  type="button"
                  className="tool-group-toggle"
                  onClick={() => toggleToolGroup(group.id)}
                  aria-expanded={!isCollapsed}
                  aria-controls={groupBodyId}
                  aria-label={isCollapsed ? "Expand tool calls" : "Collapse tool calls"}
                >
                  <span className="tool-group-chevron" aria-hidden>
                    <ChevronIcon size={14} />
                  </span>
                  <span className="tool-group-summary">{summaryText}</span>
                </button>
              </div>
              {!isCollapsed && (
                <div className="tool-group-body" id={groupBodyId}>
                  {group.items.map(renderItem)}
                </div>
              )}
            </div>
          ),
        });
        return;
      }
      nextRows.push({
        id: entry.item.id,
        node: renderItem(entry.item),
      });
    });

    if (planFollowupNode) {
      nextRows.push({
        id: `plan-followup-${threadId ?? "none"}-${planFollowup.planItemId ?? "none"}`,
        node: planFollowupNode,
      });
    }
    if (userInputNode) {
      nextRows.push({
        id: `user-input-${activeUserInputRequestId ?? "none"}`,
        node: userInputNode,
      });
    }

    nextRows.push({
      id: `working-${threadId ?? "none"}-${isThinking ? "thinking" : lastDurationMs ?? "idle"}`,
      node: (
        <WorkingIndicator
          isThinking={isThinking}
          processingStartedAt={processingStartedAt}
          lastDurationMs={lastDurationMs}
          hasItems={items.length > 0}
          reasoningLabel={workingReasoningLabel}
        />
      ),
    });

    if (!items.length && !userInputNode && !isThinking && !isLoadingMessages) {
      nextRows.push({
        id: `empty-${threadId ?? "new"}`,
        node: (
          <div className="empty messages-empty">
            {threadId ? "Send a prompt to the agent." : "Send a prompt to start a new agent."}
          </div>
        ),
      });
    }

    if (!items.length && !userInputNode && !isThinking && isLoadingMessages) {
      nextRows.push({
        id: `loading-${threadId ?? "new"}`,
        node: (
          <div className="empty messages-empty">
            <div className="messages-loading-indicator" role="status" aria-live="polite">
              <span className="working-spinner" aria-hidden />
              <span className="messages-loading-label">Loading…</span>
            </div>
          </div>
        ),
      });
    }

    return nextRows;
  }, [
    activeUserInputRequestId,
    collapsedToolGroups,
    groupedItems,
    isLoadingMessages,
    isThinking,
    items.length,
    lastDurationMs,
    planFollowup.planItemId,
    planFollowupNode,
    processingStartedAt,
    renderItem,
    threadId,
    toggleToolGroup,
    userInputNode,
    workingReasoningLabel,
  ]);
  const shouldVirtualize = rows.length > VIRTUALIZATION_ROW_THRESHOLD;

  const rowVirtualizer = useVirtualizer({
    count: rows.length,
    enabled: shouldVirtualize,
    getScrollElement: () => containerRef.current,
    estimateSize: () => 180,
    overscan: 6,
  });
  const virtualRows = rowVirtualizer.getVirtualItems();

  useEffect(() => {
    if (!shouldVirtualize) {
      return;
    }
    rowVirtualizer.measure();
  }, [
    activeUserInputRequestId,
    collapsedToolGroups,
    expandedItems,
    isThinking,
    lastDurationMs,
    planFollowup.planItemId,
    rowVirtualizer,
    scrollKey,
    shouldVirtualize,
  ]);

  return (
    <div
      className="messages messages-full"
      ref={containerRef}
      onScroll={updateAutoScroll}
    >
      {shouldVirtualize ? (
        <div
          style={{
            height: rowVirtualizer.getTotalSize(),
            position: "relative",
            width: "100%",
          }}
        >
          {virtualRows.map((virtualRow) => {
            const row = rows[virtualRow.index];
            if (!row) {
              return null;
            }
            return (
              <div
                key={virtualRow.key}
                data-index={virtualRow.index}
                ref={rowVirtualizer.measureElement}
                style={{
                  left: 0,
                  position: "absolute",
                  top: 0,
                  transform: `translateY(${virtualRow.start}px)`,
                  width: "100%",
                }}
              >
                {row.node}
              </div>
            );
          })}
        </div>
      ) : (
        rows.map((row) => <div key={row.id}>{row.node}</div>)
      )}
    </div>
  );
});
