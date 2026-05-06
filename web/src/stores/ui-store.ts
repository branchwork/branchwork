import { create } from "zustand";

/// Tiny store for chrome-level UI state that survives route changes but
/// has no server-side equivalent — kept separate from data slices so a
/// resetAllStores() on logout doesn't have to touch it (closed sidebar
/// on logout would surprise nobody).
///
/// Today this only carries the mobile sidebar's open/closed flag. The
/// flag is meaningless on `≥md` viewports (the sidebar is always
/// visible there) but we track it unconditionally so resize
/// transitions are smooth — components key off Tailwind responsive
/// utilities, not the flag, for desktop layout.
///
/// Auto-close on route change is wired at the consumer (Sidebar.tsx
/// uses a useEffect on useLocation.pathname) rather than here, so the
/// store stays decoupled from react-router.
interface UiStore {
  sidebarOpen: boolean;
  openSidebar: () => void;
  closeSidebar: () => void;
  toggleSidebar: () => void;
}

export const useUiStore = create<UiStore>((set) => ({
  sidebarOpen: false,
  openSidebar: () => set({ sidebarOpen: true }),
  closeSidebar: () => set({ sidebarOpen: false }),
  toggleSidebar: () => set((s) => ({ sidebarOpen: !s.sidebarOpen })),
}));
