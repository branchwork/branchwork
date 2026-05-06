import { beforeEach, describe, expect, it } from "vitest";
import { useUiStore } from "./ui-store.js";

beforeEach(() => {
  useUiStore.setState({ sidebarOpen: false });
});

describe("ui-store", () => {
  it("starts with the sidebar closed", () => {
    expect(useUiStore.getState().sidebarOpen).toBe(false);
  });

  it("openSidebar sets sidebarOpen to true; closeSidebar resets it", () => {
    useUiStore.getState().openSidebar();
    expect(useUiStore.getState().sidebarOpen).toBe(true);
    useUiStore.getState().closeSidebar();
    expect(useUiStore.getState().sidebarOpen).toBe(false);
  });

  it("toggleSidebar flips sidebarOpen each call", () => {
    const { toggleSidebar } = useUiStore.getState();
    toggleSidebar();
    expect(useUiStore.getState().sidebarOpen).toBe(true);
    toggleSidebar();
    expect(useUiStore.getState().sidebarOpen).toBe(false);
    toggleSidebar();
    expect(useUiStore.getState().sidebarOpen).toBe(true);
  });

  it("openSidebar is idempotent (already-open call stays open)", () => {
    useUiStore.getState().openSidebar();
    useUiStore.getState().openSidebar();
    expect(useUiStore.getState().sidebarOpen).toBe(true);
  });
});
