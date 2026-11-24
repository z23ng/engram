import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { Toaster } from "sonner";
import { App } from "./App";
import "./index.css";

const queries = new QueryClient({ defaultOptions: { queries: { retry: false, staleTime: 5_000 } } });

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <QueryClientProvider client={queries}>
      <App />
      <Toaster position="bottom-right" richColors closeButton />
    </QueryClientProvider>
  </StrictMode>,
);
