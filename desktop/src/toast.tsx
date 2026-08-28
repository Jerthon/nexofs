import { createContext, useCallback, useContext, useRef, useState } from "react";

type ToastKind = "success" | "error";
interface ToastItem {
  id: number;
  kind: ToastKind;
  message: string;
}

const ToastContext = createContext<{ push: (kind: ToastKind, message: string) => void } | null>(null);

export function ToastProvider({ children }: { children: React.ReactNode }) {
  const [toasts, setToasts] = useState<ToastItem[]>([]);
  const nextId = useRef(0);

  const push = useCallback((kind: ToastKind, message: string) => {
    const id = nextId.current++;
    setToasts((current) => [...current, { id, kind, message }]);
    setTimeout(() => setToasts((current) => current.filter((t) => t.id !== id)), 5000);
  }, []);

  const dismiss = (id: number) => setToasts((current) => current.filter((t) => t.id !== id));

  return (
    <ToastContext.Provider value={{ push }}>
      {children}
      <div className="toast-container">
        {toasts.map((t) => (
          <div key={t.id} className={`toast toast-${t.kind}`} onClick={() => dismiss(t.id)}>
            {t.message}
          </div>
        ))}
      </div>
    </ToastContext.Provider>
  );
}

function useToast() {
  const ctx = useContext(ToastContext);
  if (!ctx) throw new Error("useToast precisa estar dentro de <ToastProvider>");
  return ctx;
}

/** Toda ação disparada por um botão passa por aqui: mostra "carregando"
 * (`pending`, para desabilitar o botão e não deixar parecer que nada
 * aconteceu ao clicar), toast de sucesso/erro ao terminar, e chama
 * `onSuccess` (tipicamente um `reload`) só quando a chamada realmente deu
 * certo. */
export function useAction<Args extends unknown[], R>(action: (...args: Args) => Promise<R>, options?: { successMessage?: string; onSuccess?: () => void }) {
  const { push } = useToast();
  const [pending, setPending] = useState(false);

  const run = useCallback(
    async (...args: Args): Promise<R | undefined> => {
      setPending(true);
      try {
        const result = await action(...args);
        if (options?.successMessage) push("success", options.successMessage);
        options?.onSuccess?.();
        return result;
      } catch (err) {
        push("error", String(err));
        return undefined;
      } finally {
        setPending(false);
      }
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [action],
  );

  return { run, pending };
}
