import React, { useState, useEffect } from 'react';
import { Box, Text, useInput } from 'ink';
import { t } from '../utils/i18n.js';
import { colors } from '../utils/ui.js';
import { startDaemon, stopDaemon, getDaemonStatus } from '../utils/daemon.js';
import { LogsView } from './LogsView.js';

interface DaemonMenuProps {
  onBack: () => void;
}

export function DaemonMenu({ onBack }: DaemonMenuProps) {
  const [selectedIndex, setSelectedIndex] = useState(0);
  const [status, setStatus] = useState<'online' | 'offline' | 'starting'>('offline');
  const [port, setPort] = useState<number | undefined>(undefined);
  const [memoryCount, setMemoryCount] = useState(0);
  const [message, setMessage] = useState('');
  const [showLogs, setShowLogs] = useState(false);

  useEffect(() => {
    refreshStatus();
  }, []);

  const refreshStatus = async () => {
    const daemonStatus = await getDaemonStatus();
    setStatus(daemonStatus.online ? 'online' : 'offline');
    setPort(daemonStatus.port);
    setMemoryCount(daemonStatus.memoryCount || 0);
  };

  const handleStart = async () => {
    setMessage(t('daemon.starting'));
    const result = await startDaemon();
    if (result.success) {
      setMessage(colors.success(t('common.success')));
      await refreshStatus();
    } else {
      setMessage(colors.error(result.error || t('common.error')));
    }
    setTimeout(() => setMessage(''), 3000);
  };

  const handleStop = async () => {
    const result = await stopDaemon();
    if (result.success) {
      setMessage(colors.success(t('common.success')));
      await refreshStatus();
    } else {
      setMessage(colors.error(result.error || t('common.error')));
    }
    setTimeout(() => setMessage(''), 3000);
  };

  const menuItems = [
    { id: 'start', label: t('daemon.start'), action: handleStart, showWhen: 'offline' },
    { id: 'stop', label: t('daemon.stop'), action: handleStop, showWhen: 'online' },
    { id: 'restart', label: t('daemon.restart'), action: async () => { await handleStop(); await handleStart(); }, showWhen: 'online' },
    { id: 'logs', label: t('daemon.logs'), action: () => setShowLogs(true), showWhen: 'always' },
    { id: 'back', label: t('daemon.back'), action: onBack, showWhen: 'always' },
  ];

  const visibleItems = menuItems.filter(item => 
    item.showWhen === 'always' || item.showWhen === status
  );

  useInput((input, key) => {
    if (key.upArrow) {
      setSelectedIndex(prev => (prev > 0 ? prev - 1 : visibleItems.length - 1));
    } else if (key.downArrow) {
      setSelectedIndex(prev => (prev < visibleItems.length - 1 ? prev + 1 : 0));
    } else if (key.return) {
      visibleItems[selectedIndex].action();
    } else if (input === 'q' || key.escape) {
      onBack();
    }
  });

  const statusIcon = status === 'online' ? '🟢' : '🔴';
  const statusText = status === 'online' ? t('daemon.online') : t('daemon.offline');

  if (showLogs) {
    return <LogsView onBack={() => setShowLogs(false)} />;
  }

  return (
    <Box flexDirection="column" padding={1}>
      <Box flexDirection="column" borderStyle="round" borderColor="cyan" paddingX={2} paddingY={1}>
        <Text bold>{t('daemon.title')}</Text>
        
        <Box flexDirection="column" marginTop={1}>
          <Text>{`${t('daemon.status')}: ${statusIcon} ${statusText}`}</Text>
          {port && <Text dimColor>{`${t('daemon.port')}: ${port}`}</Text>}
          {status === 'online' && <Text dimColor>{`${t('daemon.memory')}: ${memoryCount}`}</Text>}
        </Box>

        {message && (
          <Box marginTop={1}>
            <Text>{message}</Text>
          </Box>
        )}

        <Box flexDirection="column" marginTop={1}>
          {visibleItems.map((item, index) => (
            <Text key={item.id}>
              {index === selectedIndex ? colors.primary('❯ ') : '  '}
              {index === selectedIndex ? colors.primary(item.label) : item.label}
            </Text>
          ))}
        </Box>
      </Box>

      <Box marginTop={1}>
        <Text dimColor>{t('menu.hint')}</Text>
      </Box>
    </Box>
  );
}
