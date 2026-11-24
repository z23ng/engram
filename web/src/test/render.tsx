import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render } from "@testing-library/react";
import type { ReactNode } from "react";
import { vi } from "vitest";
import type { Client } from "@/lib/api";
import { AppContext, type AppState } from "@/lib/app";

/** Renders `ui` with a stub client (only the methods a test passes) and a dev key by default. */
export function renderWithApp(ui: ReactNode, overrides: Partial<Omit<AppState, "client">> & { client: Partial<Client> }) {
  const queries = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const state: AppState = {
    me: { name: "dev", role: "dev", projects: null },
    route: { project: "alpha", view: "files", branch: "main" },
    go: vi.fn(),
    theme: "light",
    setTheme: vi.fn(),
    signOut: vi.fn(),
    ...overrides,
    client: overrides.client as Client,
  };
  render(
    <QueryClientProvider client={queries}>
      <AppContext.Provider value={state}>{ui}</AppContext.Provider>
    </QueryClientProvider>,
  );
  return state;
}
