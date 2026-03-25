import { type CSSProperties, type FormEvent, useEffect, useMemo, useState } from 'react';
import { Sidebar } from './components/Sidebar';
import { AgentCanvas } from './components/AgentCanvas';
import { HubDashboard } from './components/HubDashboard';
import { Inspector, type InspectableItem } from './components/Inspector';
import { getApiErrorMessage, tachiApi, type MemoryEntry } from './services/api';

type TabKey = 'kanban' | 'ghost' | 'memory' | 'hub' | 'settings';

const TAB_TITLES: Record<TabKey, string> = {
  kanban: 'Kanban Flow',
  ghost: 'Ghost Whispers',
  memory: 'Memory Explorer',
  hub: 'Hub Dashboard',
  settings: 'Settings',
};

function App() {
  const [activeTab, setActiveTab] = useState<TabKey>('kanban');
  const [selectedNode, setSelectedNode] = useState<InspectableItem | null>(null);
  const [daemonOnline, setDaemonOnline] = useState(false);
  const [daemonMessage, setDaemonMessage] = useState('Probing daemon connection...');
  const [memoryQuery, setMemoryQuery] = useState('agent');
  const [memoryResults, setMemoryResults] = useState<MemoryEntry[]>([]);
  const [memoryLoading, setMemoryLoading] = useState(false);
  const [memoryError, setMemoryError] = useState<string | null>(null);

  useEffect(() => {
    setSelectedNode(null);
  }, [activeTab]);

  useEffect(() => {
    let cancelled = false;

    const checkDaemon = async () => {
      try {
        await tachiApi.ping();
        if (!cancelled) {
          setDaemonOnline(true);
          setDaemonMessage('Connected to Tachi daemon on localhost:8080');
        }
      } catch (error) {
        if (!cancelled) {
          setDaemonOnline(false);
          setDaemonMessage(getApiErrorMessage(error));
        }
      }
    };

    void checkDaemon();
    const intervalId = window.setInterval(() => {
      void checkDaemon();
    }, 7000);

    return () => {
      cancelled = true;
      window.clearInterval(intervalId);
    };
  }, []);

  const runMemorySearch = async (query: string) => {
    const trimmed = query.trim();
    if (!trimmed) {
      setMemoryResults([]);
      setMemoryError(null);
      return;
    }

    setMemoryLoading(true);
    try {
      const results = await tachiApi.searchMemory(trimmed, 12);
      setMemoryResults(results);
      setMemoryError(null);
    } catch (error) {
      setMemoryResults([]);
      setMemoryError(getApiErrorMessage(error));
    } finally {
      setMemoryLoading(false);
    }
  };

  useEffect(() => {
    if (activeTab === 'memory') {
      void runMemorySearch(memoryQuery);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeTab]);

  const subtitle = useMemo(() => {
    if (daemonOnline) {
      return 'Live visualization of agent interactions';
    }
    return 'Daemon unavailable - showing degraded state';
  }, [daemonOnline]);

  const handleMemorySubmit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    void runMemorySearch(memoryQuery);
  };

  const statusDotStyle: CSSProperties = daemonOnline
    ? {}
    : {
        backgroundColor: 'var(--accent-magenta)',
        animation: 'none',
      };

  return (
    <div className="app-container">
      <Sidebar
        activeTab={activeTab}
        setActiveTab={(tab) => setActiveTab(tab as TabKey)}
        daemonOnline={daemonOnline}
      />

      <main className="glass-panel main-canvas">
        <header style={{ padding: '20px', borderBottom: '1px solid var(--glass-border)' }}>
          <h2>
            <span className="status-dot" style={statusDotStyle}></span>
            {TAB_TITLES[activeTab]}
          </h2>
          <div style={{ display: 'flex', alignItems: 'center', gap: 10, flexWrap: 'wrap' }}>
            <p className="text-muted">{subtitle}</p>
            <span
              className="tag"
              style={
                daemonOnline
                  ? undefined
                  : {
                      background: 'rgba(255, 0, 102, 0.1)',
                      color: 'var(--accent-magenta)',
                      borderColor: 'rgba(255, 0, 102, 0.3)',
                    }
              }
            >
              {daemonOnline ? 'Online' : 'Offline'}
            </span>
            {!daemonOnline && (
              <span className="text-muted" style={{ fontSize: '0.78rem' }}>
                {daemonMessage}
              </span>
            )}
          </div>
        </header>

        <div style={{ flex: 1, position: 'relative', minHeight: 0 }}>
          {(activeTab === 'kanban' || activeTab === 'ghost') && (
            <AgentCanvas view={activeTab} onNodeClick={setSelectedNode} />
          )}

          {activeTab === 'memory' && (
            <div style={{ padding: 20, height: '100%', overflowY: 'auto' }}>
              <form onSubmit={handleMemorySubmit} style={{ display: 'flex', gap: 10, marginBottom: 16 }}>
                <input
                  value={memoryQuery}
                  onChange={(event) => setMemoryQuery(event.target.value)}
                  placeholder="Search memory..."
                  style={{
                    flex: 1,
                    minWidth: 0,
                    background: 'rgba(255,255,255,0.03)',
                    border: '1px solid var(--glass-border)',
                    borderRadius: 8,
                    color: 'var(--text-main)',
                    padding: '10px 12px',
                    fontFamily: 'Outfit, sans-serif',
                  }}
                />
                <button
                  type="submit"
                  style={{
                    background: 'rgba(0,240,255,0.15)',
                    color: 'var(--accent-cyan)',
                    border: '1px solid rgba(0,240,255,0.35)',
                    borderRadius: 8,
                    padding: '10px 14px',
                    cursor: 'pointer',
                    fontWeight: 600,
                  }}
                >
                  Search
                </button>
              </form>

              {memoryLoading && <p className="text-muted">Searching memories...</p>}
              {memoryError && (
                <div className="kanban-card">
                  <span
                    className="tag"
                    style={{
                      background: 'rgba(255, 0, 102, 0.1)',
                      color: 'var(--accent-magenta)',
                      borderColor: 'rgba(255, 0, 102, 0.3)',
                    }}
                  >
                    Offline
                  </span>
                  <p className="text-muted" style={{ marginTop: 8 }}>
                    {memoryError}
                  </p>
                </div>
              )}

              {memoryResults.map((entry) => (
                <div
                  key={entry.id}
                  className="kanban-card"
                  onClick={() =>
                    setSelectedNode({
                      id: entry.id,
                      label: entry.summary || entry.path || entry.id,
                      kind: 'memory',
                      details: {
                        path: entry.path,
                        scope: entry.scope,
                        category: entry.category,
                        score: entry.score,
                        timestamp: entry.timestamp,
                        text: entry.text,
                      },
                    })
                  }
                  style={{ cursor: 'pointer' }}
                >
                  <div style={{ display: 'flex', justifyContent: 'space-between', marginBottom: 6, gap: 12 }}>
                    <strong style={{ overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                      {entry.summary || entry.id}
                    </strong>
                    <span className="text-muted" style={{ fontSize: '0.8rem' }}>
                      {entry.score ?? 'n/a'}
                    </span>
                  </div>
                  <div className="text-muted" style={{ fontSize: '0.82rem' }}>
                    {entry.path ?? '/'}
                  </div>
                </div>
              ))}

              {!memoryLoading && memoryResults.length === 0 && !memoryError && (
                <p className="text-muted">No memory matches.</p>
              )}
            </div>
          )}

          {activeTab === 'hub' && <HubDashboard onSelectItem={setSelectedNode} />}

          {activeTab === 'settings' && (
            <div style={{ padding: 20 }}>
              <div className="kanban-card">
                <h3 style={{ marginBottom: 8 }}>Daemon Endpoint</h3>
                <p className="text-muted">{daemonMessage}</p>
                <p className="text-muted" style={{ marginTop: 10, fontSize: '0.82rem' }}>
                  Override with `VITE_TACHI_BASE_URL` if your daemon runs on a custom host.
                </p>
              </div>
            </div>
          )}
        </div>
      </main>

      <Inspector selectedNode={selectedNode} />
    </div>
  );
}

export default App;
