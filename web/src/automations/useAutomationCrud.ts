import { createAutomation, deleteAutomation, updateAutomation } from "../api";
import type { Automation } from "../types";

interface Options {
  selectedId: string | null;
  setAutomations: React.Dispatch<React.SetStateAction<Automation[]>>;
  setSelectedId: (id: string | null) => void;
  setError: (error: string | null) => void;
}

/** Create / delete / rename / enable handlers for the automation list. */
export function useAutomationCrud({ selectedId, setAutomations, setSelectedId, setError }: Options) {
  const handleAdd = async () => {
    setError(null);
    try {
      const created = await createAutomation("Untitled automation");
      setAutomations((prev) => [...prev, created]);
      setSelectedId(created.id);
    } catch (e) {
      setError(String(e));
    }
  };

  const handleDelete = async (id: string) => {
    if (!confirm("Delete this automation?")) return;
    try {
      await deleteAutomation(id);
      setAutomations((prev) => prev.filter((a) => a.id !== id));
      if (selectedId === id) setSelectedId(null);
    } catch (e) {
      setError(String(e));
    }
  };

  const handleRename = async (id: string, name: string) => {
    try {
      const updated = await updateAutomation(id, { name });
      setAutomations((prev) => prev.map((a) => (a.id === id ? updated : a)));
    } catch (e) {
      setError(String(e));
    }
  };

  const handleToggleEnabled = async (id: string, enabled: boolean) => {
    try {
      const updated = await updateAutomation(id, { enabled });
      setAutomations((prev) => prev.map((a) => (a.id === id ? updated : a)));
    } catch (e) {
      setError(String(e));
    }
  };

  const handleRenameLocal = (id: string, name: string) => {
    setAutomations((prev) => prev.map((a) => (a.id === id ? { ...a, name } : a)));
  };

  return { handleAdd, handleDelete, handleRename, handleToggleEnabled, handleRenameLocal };
}
