import React, { useState, useEffect } from 'react';
import { Box, Text, useInput, useApp } from 'ink';
import { banner, colors } from '../utils/ui.js';
import { t, setLanguage } from '../utils/i18n.js';
import { loadConfig } from '../utils/config.js';
import { getDaemonStatus } from '../utils/daemon.js';
import { DaemonMenu } from './DaemonMenu.js';
import { McpMenu } from './McpMenu.js';
import { SkillsMenu } from './SkillsMenu.js';
import { MemoryMenu } from './MemoryMenu.js';
import { SettingsMenu } from './SettingsMenu.js';

type MenuView = 'main' | 'daemon' | 'mcp' | 'skills' | 'memory' | 'settings' | 'exit';

export function MainMenu() {
  const { exit } = useApp();
  const [currentView, setCurrentView] = useState<MenuView>('main');
  const [selectedIndex, setSelectedIndex] = useState(0);
  const [daemonStatus, setDaemonStatus] = useState<'online' | 'offline' | 'starting'>('offline');
  const [memoryCount, setMemoryCount] = useState(0);

  useEffect(() => {
    const config = loadConfig();
    setLanguage(config.ui.language);
    refreshStatus();
  }, []);

  const refreshStatus = () => {
    getDaemonStatus().then(status => {
      setDaemonStatus(status.online ? 'online' : 'offline');
      setMemoryCount(status.memoryCount || 0);
    });
  };

  const menuItems = [
    { id: 'daemon' as MenuView, label: t('menu.daemon') },
    { id: 'mcp' as MenuView, label: t('menu.mcp') },
    { id: 'skills' as MenuView, label: t('menu.skills') },
    { id: 'memory' as MenuView, label: t('menu.memory') },
    { id: 'settings' as MenuView, label: t('menu.settings') },
    { id: 'exit' as MenuView, label: t('menu.exit') },
  ];

  useInput((input, key) => {
    if (currentView !== 'main') return;
    
    if (key.upArrow) {
      setSelectedIndex(prev => (prev > 0 ? prev - 1 : menuItems.length - 1));
    } else if (key.downArrow) {
      setSelectedIndex(prev => (prev < menuItems.length - 1 ? prev + 1 : 0));
    } else if (key.return) {
      const selected = menuItems[selectedIndex].id;
      if (selected === 'exit') {
        exit();
      } else {
        setCurrentView(selected);
      }
    } else if (input === 'q' || key.escape) {
      exit();
    }
  });

  const handleBack = () => {
    setCurrentView('main');
    refreshStatus();
  };

  // Render submenus
  if (currentView === 'daemon') {
    return <DaemonMenu onBack={handleBack} />;
  }
  if (currentView === 'mcp') {
    return <McpMenu onBack={handleBack} />;
  }
  if (currentView === 'skills') {
    return <SkillsMenu onBack={handleBack} />;
  }
  if (currentView === 'memory') {
    return <MemoryMenu onBack={handleBack} />;
  }
  if (currentView === 'settings') {
    return <SettingsMenu onBack={handleBack} />;
  }

  // Main menu
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
