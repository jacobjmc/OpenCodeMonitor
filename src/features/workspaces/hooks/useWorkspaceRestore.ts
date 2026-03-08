import { useEffect, useRef } from "react";
import type { WorkspaceInfo } from "../../../types";

const RESTORE_RETRY_DELAY_MS = 1500;
const MAX_RESTORE_RETRIES = 5;

type WorkspaceRestoreOptions = {
  workspaces: WorkspaceInfo[];
  hasLoaded: boolean;
  enabled?: boolean;
  connectWorkspace: (workspace: WorkspaceInfo) => Promise<void>;
  listThreadsForWorkspace: (
    workspace: WorkspaceInfo,
    options?: { preserveState?: boolean },
  ) => Promise<void>;
};

export function useWorkspaceRestore({
  workspaces,
  hasLoaded,
  enabled = true,
  connectWorkspace,
  listThreadsForWorkspace,
}: WorkspaceRestoreOptions) {
  const restoredWorkspaces = useRef(new Set<string>());
  const restoringWorkspaces = useRef(new Set<string>());
  const retryCountByWorkspace = useRef(new Map<string, number>());
  const retryTimers = useRef(new Map<string, ReturnType<typeof setTimeout>>());
  const optionsRef = useRef({
    workspaces,
    hasLoaded,
    enabled,
    connectWorkspace,
    listThreadsForWorkspace,
  });

  useEffect(() => {
    optionsRef.current = {
      workspaces,
      hasLoaded,
      enabled,
      connectWorkspace,
      listThreadsForWorkspace,
    };
  });

  useEffect(
    () => () => {
      retryTimers.current.forEach((timer) => {
        clearTimeout(timer);
      });
      retryTimers.current.clear();
    },
    [],
  );

  useEffect(() => {
    if (!hasLoaded || !enabled) {
      return;
    }

    const activeWorkspaceIds = new Set(workspaces.map((workspace) => workspace.id));
    restoredWorkspaces.current.forEach((workspaceId) => {
      if (!activeWorkspaceIds.has(workspaceId)) {
        restoredWorkspaces.current.delete(workspaceId);
      }
    });
    restoringWorkspaces.current.forEach((workspaceId) => {
      if (!activeWorkspaceIds.has(workspaceId)) {
        restoringWorkspaces.current.delete(workspaceId);
      }
    });
    retryCountByWorkspace.current.forEach((_count, workspaceId) => {
      if (!activeWorkspaceIds.has(workspaceId)) {
        retryCountByWorkspace.current.delete(workspaceId);
      }
    });
    retryTimers.current.forEach((timer, workspaceId) => {
      if (activeWorkspaceIds.has(workspaceId)) {
        return;
      }
      clearTimeout(timer);
      retryTimers.current.delete(workspaceId);
    });

    const restoreWorkspace = (workspaceId: string) => {
      const {
        workspaces: latestWorkspaces,
        hasLoaded: latestHasLoaded,
        enabled: latestEnabled,
        connectWorkspace: latestConnectWorkspace,
        listThreadsForWorkspace: latestListThreadsForWorkspace,
      } = optionsRef.current;
      if (!latestHasLoaded || !latestEnabled) {
        return;
      }
      const workspace = latestWorkspaces.find((entry) => entry.id === workspaceId);
      if (!workspace) {
        return;
      }
      if (restoredWorkspaces.current.has(workspaceId)) {
        return;
      }
      if (restoringWorkspaces.current.has(workspaceId)) {
        return;
      }

      restoringWorkspaces.current.add(workspaceId);
      void (async () => {
        try {
          if (!workspace.connected) {
            await latestConnectWorkspace(workspace);
          }
          await latestListThreadsForWorkspace(workspace);
          restoredWorkspaces.current.add(workspaceId);
          retryCountByWorkspace.current.delete(workspaceId);
          const retryTimer = retryTimers.current.get(workspaceId);
          if (retryTimer) {
            clearTimeout(retryTimer);
            retryTimers.current.delete(workspaceId);
          }
        } catch {
          const attempts = retryCountByWorkspace.current.get(workspaceId) ?? 0;
          if (attempts >= MAX_RESTORE_RETRIES) {
            return;
          }
          retryCountByWorkspace.current.set(workspaceId, attempts + 1);
          if (!retryTimers.current.has(workspaceId)) {
            const timer = setTimeout(() => {
              retryTimers.current.delete(workspaceId);
              restoreWorkspace(workspaceId);
            }, RESTORE_RETRY_DELAY_MS);
            retryTimers.current.set(workspaceId, timer);
          }
        } finally {
          restoringWorkspaces.current.delete(workspaceId);
        }
      })();
    };

    workspaces.forEach((workspace) => {
      restoreWorkspace(workspace.id);
    });
  }, [connectWorkspace, enabled, hasLoaded, listThreadsForWorkspace, workspaces]);
}
