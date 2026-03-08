// @vitest-environment jsdom
import { act, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { WorkspaceInfo } from "../../../types";
import { useWorkspaceRestore } from "./useWorkspaceRestore";

const RETRY_DELAY_MS = 1500;

function flushMicrotasks() {
  return Promise.resolve().then(() => Promise.resolve());
}

function createWorkspace(overrides?: Partial<WorkspaceInfo>): WorkspaceInfo {
  return {
    id: "ws-1",
    name: "Workspace",
    path: "/tmp/workspace",
    connected: false,
    kind: "main",
    parentId: null,
    worktree: null,
    settings: { sidebarCollapsed: false },
    ...overrides,
  };
}

describe("useWorkspaceRestore", () => {
  afterEach(() => {
    vi.useRealTimers();
    vi.clearAllMocks();
  });

  it("retries connect/list after a startup failure", async () => {
    vi.useFakeTimers();
    const workspace = createWorkspace();
    const connectWorkspace = vi
      .fn<WorkspaceRestoreOptions["connectWorkspace"]>()
      .mockRejectedValueOnce(new Error("temporary failure"))
      .mockResolvedValue(undefined);
    const listThreadsForWorkspace = vi
      .fn<WorkspaceRestoreOptions["listThreadsForWorkspace"]>()
      .mockResolvedValue(undefined);

    renderHook(() =>
      useWorkspaceRestore({
        workspaces: [workspace],
        hasLoaded: true,
        connectWorkspace,
        listThreadsForWorkspace,
      }),
    );

    await act(async () => {
      await flushMicrotasks();
    });

    expect(connectWorkspace).toHaveBeenCalledTimes(1);
    expect(listThreadsForWorkspace).toHaveBeenCalledTimes(0);

    await act(async () => {
      vi.advanceTimersByTime(RETRY_DELAY_MS);
      await flushMicrotasks();
    });

    expect(connectWorkspace).toHaveBeenCalledTimes(2);
    expect(listThreadsForWorkspace).toHaveBeenCalledTimes(1);
  });

  it("does not start duplicate reconnects while one is in flight", async () => {
    vi.useFakeTimers();
    const workspace = createWorkspace();
    let resolveConnect: (() => void) | null = null;
    const connectWorkspace = vi.fn<WorkspaceRestoreOptions["connectWorkspace"]>(() =>
      new Promise<void>((resolve) => {
        resolveConnect = resolve;
      }),
    );
    const listThreadsForWorkspace = vi
      .fn<WorkspaceRestoreOptions["listThreadsForWorkspace"]>()
      .mockResolvedValue(undefined);

    const { rerender } = renderHook(
      ({ workspaces }) =>
        useWorkspaceRestore({
          workspaces,
          hasLoaded: true,
          connectWorkspace,
          listThreadsForWorkspace,
        }),
      {
        initialProps: { workspaces: [workspace] as WorkspaceInfo[] },
      },
    );

    await act(async () => {
      await flushMicrotasks();
    });

    rerender({ workspaces: [{ ...workspace }] });

    await act(async () => {
      await flushMicrotasks();
    });

    expect(connectWorkspace).toHaveBeenCalledTimes(1);

    await act(async () => {
      resolveConnect?.();
      await flushMicrotasks();
    });

    expect(listThreadsForWorkspace).toHaveBeenCalledTimes(1);
  });

  it("loads thread list on open for already connected workspaces", async () => {
    const workspace = createWorkspace({ connected: true });
    const connectWorkspace = vi.fn<WorkspaceRestoreOptions["connectWorkspace"]>();
    const listThreadsForWorkspace = vi
      .fn<WorkspaceRestoreOptions["listThreadsForWorkspace"]>()
      .mockResolvedValue(undefined);

    renderHook(() =>
      useWorkspaceRestore({
        workspaces: [workspace],
        hasLoaded: true,
        connectWorkspace,
        listThreadsForWorkspace,
      }),
    );

    await act(async () => {
      await flushMicrotasks();
    });

    expect(connectWorkspace).not.toHaveBeenCalled();
    expect(listThreadsForWorkspace).toHaveBeenCalledTimes(1);
    expect(listThreadsForWorkspace).toHaveBeenCalledWith(workspace);
  });

  it("waits until restore is enabled before reconnecting workspaces", async () => {
    const workspace = createWorkspace();
    const connectWorkspace = vi
      .fn<WorkspaceRestoreOptions["connectWorkspace"]>()
      .mockResolvedValue(undefined);
    const listThreadsForWorkspace = vi
      .fn<WorkspaceRestoreOptions["listThreadsForWorkspace"]>()
      .mockResolvedValue(undefined);

    const { rerender } = renderHook(
      ({ enabled }) =>
        useWorkspaceRestore({
          workspaces: [workspace],
          hasLoaded: true,
          enabled,
          connectWorkspace,
          listThreadsForWorkspace,
        }),
      {
        initialProps: { enabled: false },
      },
    );

    await act(async () => {
      await flushMicrotasks();
    });

    expect(connectWorkspace).not.toHaveBeenCalled();
    expect(listThreadsForWorkspace).not.toHaveBeenCalled();

    rerender({ enabled: true });

    await act(async () => {
      await flushMicrotasks();
    });

    expect(connectWorkspace).toHaveBeenCalledTimes(1);
    expect(listThreadsForWorkspace).toHaveBeenCalledTimes(1);
  });
});

type WorkspaceRestoreOptions = {
  connectWorkspace: (workspace: WorkspaceInfo) => Promise<void>;
  listThreadsForWorkspace: (
    workspace: WorkspaceInfo,
    options?: { preserveState?: boolean },
  ) => Promise<void>;
};
