import { useEffect, useMemo, useState } from 'react';
import {
  getApiErrorMessage,
  tachiApi,
  type AuditLogEntry,
  type GcStats,
  type HubCapability,
} from '../services/api';
import type { InspectableItem } from './Inspector';

interface HubDashboardProps {
  onSelectItem: (item: InspectableItem) => void;
}

function formatDate(value: string | null | undefined): string {
  if (!value) {
    return 'n/a';
  }

  const parsed = new Date(value);
  if (Number.isNaN(parsed.getTime())) {
    return value;
  }

  return parsed.toLocaleString();
}

function toInspectableCapability(capability: HubCapability): InspectableItem {
  return {
    id: capability.id,
    label: capability.name ?? capability.id,
    kind: capability.cap_type === 'mcp' ? 'mcp' : capability.cap_type === 'skill' ? 'skill' : 'unknown',
    details: {
      type: capability.cap_type,
      enabled: capability.enabled,
      visibility: capability.visibility,
      version: capability.version,
      db: capability.db,
      uses: capability.uses,
      successes: capability.successes,
      failures: capability.failures,
      lastUsed: capability.last_used ?? 'never',
      description: capability.description ?? '',
    },
  };
}

function renderGcSection(gcStats: GcStats | null) {
  if (!gcStats) {
    return <p className="text-muted">No GC stats yet.</p>;
  }

  const groups = [
    { key: 'global', value: gcStats.global },
    { key: 'project', value: gcStats.project },
  ].filter((group) => group.value && typeof group.value === 'object');

  if (groups.length === 0) {
    return <p className="text-muted">No GC stats yet.</p>;
  }

  return (
    <>
      {groups.map((group) => (
        <div key={group.key} className="kanban-card">
          <div style={{ display: 'flex', justifyContent: 'space-between', marginBottom: 8 }}>
            <strong style={{ textTransform: 'capitalize' }}>{group.key}</strong>
          </div>
          <div style={{ display: 'grid', gap: 6 }}>
            {Object.entries(group.value ?? {}).map(([metric, value]) => (
              <div
                key={`${group.key}-${metric}`}
                style={{ display: 'flex', justifyContent: 'space-between', gap: 12 }}
              >
                <span className="text-muted" style={{ fontSize: '0.82rem' }}>
                  {metric}
                </span>
                <span style={{ fontFamily: 'monospace' }}>{String(value)}</span>
              </div>
            ))}
          </div>
        </div>
      ))}
    </>
  );
}

export function HubDashboard({ onSelectItem }: HubDashboardProps) {
  const [capabilities, setCapabilities] = useState<HubCapability[]>([]);
  const [auditLogs, setAuditLogs] = useState<AuditLogEntry[]>([]);
  const [gcStats, setGcStats] = useState<GcStats | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;

    const load = async () => {
      try {
        const [hubCapabilities, recentAudit, gc] = await Promise.all([
          tachiApi.fetchHubCapabilities(),
          tachiApi.fetchRecentAuditLogs(12),
          tachiApi.getGcStats(),
        ]);

        if (cancelled) {
          return;
        }

        setCapabilities(hubCapabilities);
        setAuditLogs(recentAudit);
        setGcStats(gc);
        setError(null);
      } catch (fetchError) {
        if (!cancelled) {
          setError(getApiErrorMessage(fetchError));
        }
      } finally {
        if (!cancelled) {
          setLoading(false);
        }
      }
    };

    void load();
    const intervalId = window.setInterval(() => {
      void load();
    }, 10000);

    return () => {
      cancelled = true;
      window.clearInterval(intervalId);
    };
  }, []);

  const skills = useMemo(
    () => capabilities.filter((capability) => capability.cap_type === 'skill'),
    [capabilities],
  );
  const mcpServers = useMemo(
    () => capabilities.filter((capability) => capability.cap_type === 'mcp'),
    [capabilities],
  );

  return (
    <div style={{ padding: 20, height: '100%', overflowY: 'auto' }}>
      <div
        style={{
          display: 'grid',
          gap: 16,
          gridTemplateColumns: 'repeat(auto-fit, minmax(260px, 1fr))',
        }}
      >
        <section className="glass-panel" style={{ padding: 16 }}>
          <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: 10 }}>
            <h3 style={{ margin: 0 }}>Skills</h3>
            <span className="tag">{skills.length}</span>
          </div>
          {skills.length === 0 && <p className="text-muted">No registered skills.</p>}
          {skills.map((skill) => (
            <div
              key={skill.id}
              className="kanban-card"
              style={{ cursor: 'pointer' }}
              onClick={() => onSelectItem(toInspectableCapability(skill))}
            >
              <div style={{ display: 'flex', justifyContent: 'space-between', marginBottom: 6 }}>
                <strong>{skill.name}</strong>
                <span
                  className="tag"
                  style={
                    skill.enabled
                      ? undefined
                      : {
                          background: 'rgba(255, 0, 102, 0.1)',
                          color: 'var(--accent-magenta)',
                          borderColor: 'rgba(255, 0, 102, 0.3)',
                        }
                  }
                >
                  {skill.enabled ? 'enabled' : 'disabled'}
                </span>
              </div>
              <div className="text-muted" style={{ fontSize: '0.82rem' }}>
                {skill.id}
              </div>
            </div>
          ))}
        </section>

        <section className="glass-panel" style={{ padding: 16 }}>
          <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: 10 }}>
            <h3 style={{ margin: 0 }}>MCP Servers</h3>
            <span className="tag">{mcpServers.length}</span>
          </div>
          {mcpServers.length === 0 && <p className="text-muted">No registered MCP servers.</p>}
          {mcpServers.map((server) => (
            <div
              key={server.id}
              className="kanban-card"
              style={{ cursor: 'pointer' }}
              onClick={() => onSelectItem(toInspectableCapability(server))}
            >
              <div style={{ display: 'flex', justifyContent: 'space-between', marginBottom: 6 }}>
                <strong>{server.name}</strong>
                <span
                  className="tag"
                  style={
                    server.enabled
                      ? undefined
                      : {
                          background: 'rgba(255, 0, 102, 0.1)',
                          color: 'var(--accent-magenta)',
                          borderColor: 'rgba(255, 0, 102, 0.3)',
                        }
                  }
                >
                  {server.enabled ? 'enabled' : 'disabled'}
                </span>
              </div>
              <div className="text-muted" style={{ fontSize: '0.82rem' }}>
                {server.id}
              </div>
            </div>
          ))}
        </section>

        <section className="glass-panel" style={{ padding: 16 }}>
          <h3 style={{ marginBottom: 10 }}>Audit Log</h3>
          {auditLogs.length === 0 && <p className="text-muted">No recent audit entries.</p>}
          {auditLogs.map((entry, index) => {
            const itemId = entry.id ?? `audit-${index}`;
            return (
              <div
                key={itemId}
                className="kanban-card"
                style={{ cursor: 'pointer' }}
                onClick={() =>
                  onSelectItem({
                    id: itemId,
                    label: `${entry.server_id ?? 'server'} · ${entry.tool_name ?? 'tool'}`,
                    kind: 'audit',
                    details: {
                      ...entry,
                    },
                  })
                }
              >
                <div style={{ display: 'flex', justifyContent: 'space-between', gap: 12 }}>
                  <strong style={{ overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                    {entry.server_id ?? 'unknown-server'}
                  </strong>
                  <span className="text-muted" style={{ fontSize: '0.8rem' }}>
                    {formatDate(entry.timestamp)}
                  </span>
                </div>
                <div className="text-muted" style={{ fontSize: '0.82rem', marginTop: 6 }}>
                  {entry.tool_name ?? 'unknown-tool'}
                </div>
              </div>
            );
          })}
        </section>

        <section className="glass-panel" style={{ padding: 16 }}>
          <h3 style={{ marginBottom: 10 }}>GC Stats</h3>
          {renderGcSection(gcStats)}
        </section>
      </div>

      {(loading || error) && (
        <div style={{ marginTop: 16 }}>
          {loading && <p className="text-muted">Refreshing Hub data...</p>}
          {error && (
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
          )}
        </div>
      )}
    </div>
  );
}
