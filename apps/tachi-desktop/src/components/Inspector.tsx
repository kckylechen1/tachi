export type InspectableKind = 'core' | 'agent' | 'topic' | 'skill' | 'mcp' | 'memory' | 'audit' | 'unknown';

export interface InspectableItem {
  id: string;
  label: string;
  kind: InspectableKind;
  group?: number;
  details?: Record<string, unknown>;
}

interface InspectorProps {
  selectedNode: InspectableItem | null;
}

function tagLabel(kind: InspectableKind): string {
  switch (kind) {
    case 'core':
      return 'System Core';
    case 'agent':
      return 'Agent';
    case 'topic':
      return 'Topic';
    case 'skill':
      return 'Skill';
    case 'mcp':
      return 'MCP Server';
    case 'memory':
      return 'Memory';
    case 'audit':
      return 'Audit';
    default:
      return 'Item';
  }
}

function formatValue(value: unknown): string {
  if (value === null || value === undefined) {
    return 'n/a';
  }
  if (typeof value === 'string') {
    return value;
  }
  if (typeof value === 'number' || typeof value === 'boolean') {
    return String(value);
  }
  if (Array.isArray(value)) {
    return value.map((entry) => formatValue(entry)).join(', ');
  }
  if (typeof value === 'object') {
    try {
      return JSON.stringify(value);
    } catch {
      return '[object]';
    }
  }
  return String(value);
}

export function Inspector({ selectedNode }: InspectorProps) {
  if (!selectedNode) {
    return (
      <aside className="glass-panel inspector" style={{ justifyContent: 'center', alignItems: 'center' }}>
        <p className="text-muted" style={{ textAlign: 'center' }}>
          Select a node, memory, or card to inspect details.
        </p>
      </aside>
    );
  }

  const details = Object.entries(selectedNode.details ?? {}).filter(
    ([, value]) => value !== null && value !== undefined && value !== '',
  );
  const isTopicStyle = selectedNode.kind === 'topic';

  return (
    <aside className="glass-panel inspector" style={{ display: 'flex', flexDirection: 'column' }}>
      <div style={{ marginBottom: 24 }}>
        <h3 style={{ margin: '0 0 8px 0', borderBottom: '1px solid var(--glass-border)', paddingBottom: 8 }}>
          Inspector
        </h3>
        <span className={`tag ${isTopicStyle ? 'topic' : ''}`}>{tagLabel(selectedNode.kind)}</span>
      </div>

      <div style={{ flex: 1, overflowY: 'auto' }}>
        <div style={{ marginBottom: 16 }}>
          <div className="text-muted" style={{ fontSize: '0.8rem', marginBottom: 4 }}>
            ID
          </div>
          <div
            style={{
              fontFamily: 'monospace',
              fontSize: '0.85rem',
              padding: 8,
              background: 'rgba(0,0,0,0.2)',
              borderRadius: 4,
              wordBreak: 'break-word',
            }}
          >
            {selectedNode.id}
          </div>
        </div>

        <div style={{ marginBottom: 16 }}>
          <div className="text-muted" style={{ fontSize: '0.8rem', marginBottom: 4 }}>
            Label
          </div>
          <div>{selectedNode.label}</div>
        </div>

        {details.length === 0 && (
          <div className="kanban-card">
            <p className="text-muted" style={{ fontSize: '0.85rem' }}>
              No additional metadata available.
            </p>
          </div>
        )}

        {details.map(([key, value]) => (
          <div key={key} style={{ marginBottom: 12 }}>
            <div className="text-muted" style={{ fontSize: '0.78rem', marginBottom: 4 }}>
              {key}
            </div>
            <div
              style={{
                fontSize: '0.86rem',
                padding: 8,
                borderRadius: 6,
                background: 'rgba(255,255,255,0.03)',
                border: '1px solid var(--glass-border)',
                wordBreak: 'break-word',
              }}
            >
              {formatValue(value)}
            </div>
          </div>
        ))}
      </div>
    </aside>
  );
}
