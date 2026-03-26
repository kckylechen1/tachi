import React, { useState, useEffect } from 'react';
import { Box, Text, useInput, useApp } from 'ink';
import { banner, colors } from '../utils/ui.js';
import { t, setLanguage } from '../utils/i18n.js';
import { loadConfig } from '../utils/config.js';
import { getDaemonStatus } from '../utils/daemon.js';

export function MainMenu() {
  const { exit } = useApp();
  const [selectedIndex, setSelectedIndex] = useState(0);
  const [daemonStatus, setDaemonStatus] = useState<'online' | 'offline' | 'starting'>('offline');
  const [memoryCount, setMemoryCount] = useState(0);

  useEffect(() => {
    const config = loadConfig();
    setLanguage(config.ui.language);
    
    // Check daemon status
    getDaemonStatus().then(status => {
      setDaemonStatus(status.online ? 'online' : 'offline');
      setMemoryCount(status.memoryCount || 0);
    });
  }, []);

  const menuItems = [
    { id: 'daemon', label: t('menu.daemon') },
    { id: 'mcp', label: t('menu.mcp') },
    { id: 'skills', label: t('menu.skills') },
    { id: 'memory', label: t('menu.memory') },
    { id: 'settings', label: t('menu.settings') },
    { id: 'exit', label: t('menu.exit') },
  ];

  useInput((input, key) => {
    if (key.upArrow) {
      setSelectedIndex(prev => (prev > 0 ? prev - 1 : menuItems.length - 1));
    } else if (key.downArrow) {
      setSelectedIndex(prev => (prev < menuItems.length - 1 ? prev + 1 : 0));
    } else if (key.return) {
      handleSelect(menuItems[selectedIndex].id);
    } else if (input === 'q' || key.escape) {
      exit();
    }
  });

  const handleSelect = (id: string) => {
    switch (id) {
      case 'exit':
        exit();
        break;
      case 'daemon':
        // Navigate to daemon menu
        break;
      // TODO: Implement other menu navigations
    }
  };

  const statusIcon = daemonStatus === 'online' ? '🟢' : '🔴';
  const statusText = daemonStatus === 'online' ? t('daemon.online') : t('daemon.offline');

  return (
    <Box flexDirection="column" padding={1}>
      <Text>{banner}</Text>
      
      <Box flexDirection="column" marginTop={1} marginBottom={1}>
        <Text>
          {colors.dim(`${t('daemon.status')}: ${statusIcon} ${statusText}`)}
        </Text>
        {daemonStatus === 'online' && (
          <Text>
            {colors.dim(`${t('daemon.memory')}: ${memoryCount}`)}
          </Text>
        )}
      </Box>

      <Box flexDirection="column" borderStyle="round" borderColor="cyan" paddingX={2} paddingY={1}>
        <Text bold>{t('menu.title')}</Text>
        <Box flexDirection="column" marginTop={1}>
          {menuItems.map((item, index) => (
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