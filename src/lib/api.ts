export const isTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;

type UnlistenFn = () => void;

class EventBus {
  private listeners: Map<string, Set<(payload: any) => void>> = new Map();

  emit(event: string, payload: any) {
    const set = this.listeners.get(event);
    if (set) {
      set.forEach((fn) => {
        try {
          fn({ payload });
        } catch (e) {
          console.error('Listener error:', e);
        }
      });
    }
  }

  listen(event: string, callback: (event: { payload: any }) => void): UnlistenFn {
    if (!this.listeners.has(event)) {
      this.listeners.set(event, new Set());
    }
    const set = this.listeners.get(event)!;
    set.add(callback);
    return () => {
      set.delete(callback);
    };
  }
}

export const webEventBus = new EventBus();

let webSocket: WebSocket | null = null;

function ensureWebSocket() {
  if (isTauri || typeof window === 'undefined') return;
  if (webSocket && (webSocket.readyState === WebSocket.OPEN || webSocket.readyState === WebSocket.CONNECTING)) {
    return;
  }
  const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
  const wsUrl = `${protocol}//${window.location.host}/api/ws`;
  try {
    webSocket = new WebSocket(wsUrl);
    webSocket.onmessage = (event) => {
      try {
        const msg = JSON.parse(event.data);
        if (msg.event) {
          webEventBus.emit(msg.event, msg.payload);
        }
      } catch (e) {
        console.error('WS parse error:', e);
      }
    };
    webSocket.onclose = () => {
      webSocket = null;
      setTimeout(ensureWebSocket, 3000);
    };
  } catch (e) {
    console.error('WS connection error:', e);
  }
}

export async function apiInvoke<T>(command: string, args: Record<string, any> = {}): Promise<T> {
  if (isTauri) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke<T>(command, args);
  }

  ensureWebSocket();

  const response = await fetch(`/api/${command}`, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
    },
    body: JSON.stringify(args),
  });

  if (!response.ok) {
    const errorText = await response.text();
    throw new Error(errorText || `API error ${response.status}: ${command}`);
  }

  return response.json();
}

export async function apiListen<T>(event: string, callback: (event: { payload: T }) => void): Promise<UnlistenFn> {
  if (isTauri) {
    const { listen } = await import('@tauri-apps/api/event');
    return listen<T>(event, callback);
  }

  ensureWebSocket();
  return webEventBus.listen(event, callback);
}

export async function apiGetVersion(): Promise<string> {
  if (isTauri) {
    const { getVersion } = await import('@tauri-apps/api/app');
    return getVersion();
  }
  try {
    const res = await fetch('/api/version');
    if (res.ok) {
      const data = await res.json();
      return data.version || '0.1.0-umbrel';
    }
  } catch {}
  return '0.1.0-umbrel';
}
