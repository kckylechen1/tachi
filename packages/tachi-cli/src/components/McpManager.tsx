import React, { useState, useEffect } from 'react';
import { Box, Text, useInput } from 'ink';
import { t } from '../utils/i18n.js';
import { colors } from '../utils/ui.js';
import { getMcpServers, toggleMcpServer, removeMcpServer, type McpServer } from '../utils/mcp.js';
import { McpAddForm } from './McpAddForm.js';
import { McpToolsDiscovery } from './McpToolsDiscovery.js';

interface McpManagerProps {
  onBack: () => void;
}

type ViewMode = 'list' | 'add' | 'discover';

export function McpManager({ onBack }: McpManagerProps) {
  const [servers, setServers] = useState<McpServer[]>([]);
  const [selectedIndex, setSelectedIndex] = useState(0);
  const [viewMode, setViewMode] = useState<ViewMode>('list');
  const [message, setMessage] = useState('');

  useEffect(() => {
    refreshServers();
  }, []);

  const refreshServers = () => {
    const serverList = getMcpServers();
    setServers(serverList);
  };

  const handleToggle = () => {
    if (servers.length === 0) return;
    const server = servers[selectedIndex];
    toggleMcpServer(server.id, !server.enabled);
    refreshServers();
    setMessage(`${server.name} ${!server.enabled ? 'enabled' : 'disabled'}`);
    setTimeout(() => setMessage(''), 2000);
  };

  const handleRemove = () => {
    if (servers.length === 0) return;
    const server = servers[selectedIndex];
    removeMcpServer(server.id);
    refreshServers();
    setSelectedIndex(prev => Math.min(prev, servers.length - 2));
    setMessage(`${server.name} removed`);
    setTimeout(() => setMessage(''), 2000);
  };

  const menuItems = [
    { id: 'toggle', label: 'Enable/Disable', action: handleToggle },
    { id: 'remove', label: 'Remove Server', action: handleRemove },
    { id: 'discover', label: 'Discover Tools', action: () => setViewMode('discover') },
    { id: 'add', label: t('mcp.add'), action: () => setViewMode('add') },
    { id: 'back', label: t('daemon.back'), action: onBack },
  ];

  useInput((input, key) => {
    if (viewMode !== 'list') return;
    
    if (key.upArrow) {
      if (servers.length > 0) {
        setSelectedIndex(prev => (prev > 0 ? prev - 1 : servers.length - 1));
      }
    } else if (key.downArrow) {
      if (servers.length > 0) {
        setSelectedIndex(prev => (prev < servers.length - 1 ? prev + 1 : 0));
      }
    } else if (input === 'e') {
      handleToggle();
    } else if (input === 'd') {
      handleRemove();
    } else if (input === 't') {
      setViewMode('discover');
    } else if (input === 'a') {
      setViewMode('add');
    } else if (input === 'q' || key.escape) {
      onBack();
    }
  });

  if (viewMode === 'add') {
    return (
      <McpAddForm
        onBack={() => {
          setViewMode('list');
          refreshServers();
        }}
      />
    );
  }

  if (viewMode === 'discover') {
    return (
      <McpToolsDiscovery
        onBack={() => {
          setViewMode('list');
        }}
      />
    );
  }

  return (
    <Box flexDirection="column" padding={1}>
      <Box flexDirection="column" borderStyle="round" borderColor="cyan" paddingX={2} paddingY={1}>
        <Text bold>{t('mcp.title')}</Text>
        
        {message && (
          <Box marginTop={1}>
            <Text>{colors.success(message)}</Text>
          </Box>
        )}

        <Box flexDirection="column" marginTop={1}>
          {servers.length === 0 ? (
            <Text dimColor>No MCP servers configured. Press 'a' to add one.</Text>
          ) : (
            servers.map((server, index) => (
              <Box key={server.id} flexDirection="row">
                <Text>
                  {index === selectedIndex ? colors.primary('❯ ') : '  '}
                  {server.enabled ? colors.success('●') : colors.error('○')} {server.name}
                </Text>
                <Text dimColor>  {server.command} {server.args.join(' ')}</Text>
              </Box>
            ))
          )}
        </Box>

        <Box flexDirection="column" marginTop={2}>
          <Text dimColor>Commands:</Text>
          <Text dimColor>  e - Enable/Disable selected</Text>
          <Text dimColor>  d - Delete selected</Text>
          <Text dimColor>  t - Discover tools</Text>
          <Text dimColor>  a - Add new server</Text>
          <Text dimColor>  q - Back</Text>
        </Box>
      </Box>
    </Box>
  );
}
