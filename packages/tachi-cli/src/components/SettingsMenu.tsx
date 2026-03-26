import React, { useState } from 'react';
import { Box, Text, useInput } from 'ink';
import { t, setLanguage, type Language } from '../utils/i18n.js';
import { colors } from '../utils/ui.js';
import { loadConfig, saveConfig } from '../utils/config.js';

interface SettingsMenuProps {
  onBack: () => void;
}

export function SettingsMenu({ onBack }: SettingsMenuProps) {
  const [selectedIndex, setSelectedIndex] = useState(0);
  const [message, setMessage] = useState('');
  const config = loadConfig();

  const handleLanguageChange = () => {
    const newLang: Language = config.ui.language === 'en' ? 'zh' : 'en';
    config.ui.language = newLang;
    setLanguage(newLang);
    saveConfig(config);
    setMessage(colors.success(`Language changed to ${newLang === 'en' ? 'English' : '中文'}`));
    setTimeout(() => setMessage(''), 2000);
  };

  const handleAutoStartToggle = () => {
    config.daemon.autoStart = !config.daemon.autoStart;
    saveConfig(config);
    setMessage(colors.success(`Auto-start ${config.daemon.autoStart ? 'enabled' : 'disabled'}`));
    setTimeout(() => setMessage(''), 2000);
  };

  const menuItems = [
    { 
      id: 'language', 
      label: `${t('settings.language')}: ${config.ui.language === 'en' ? 'English' : '中文'}`,
      action: handleLanguageChange 
    },
    { 
      id: 'autostart', 
      label: `${t('settings.autoStart')}: ${config.daemon.autoStart ? '✓' : '✗'}`,
      action: handleAutoStartToggle 
    },
    { 
      id: 'datadir', 
      label: `${t('settings.dataDir')}: ${config.paths.dataDir}`,
      action: () => {} 
    },
    { id: 'back', label: t('daemon.back'), action: onBack },
  ];

  useInput((input, key) => {
    if (key.upArrow) {
      setSelectedIndex(prev => (prev > 0 ? prev - 1 : menuItems.length - 1));
    } else if (key.downArrow) {
      setSelectedIndex(prev => (prev < menuItems.length - 1 ? prev + 1 : 0));
    } else if (key.return) {
      menuItems[selectedIndex].action();
    } else if (input === 'q' || key.escape) {
      onBack();
    }
  });

  return (
    <Box flexDirection="column" padding={1}>
      <Box flexDirection="column" borderStyle="round" borderColor="cyan" paddingX={2} paddingY={1}>
        <Text bold>{t('settings.title')}</Text>

        {message && (
          <Box marginTop={1}>
            <Text>{message}</Text>
          </Box>
        )}
        
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
