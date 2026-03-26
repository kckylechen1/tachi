import React, { useState } from 'react';
import { Box, Text, useInput } from 'ink';
import { t } from '../utils/i18n.js';
import { colors } from '../utils/ui.js';

interface MemoryMenuProps {
  onBack: () => void;
}

export function MemoryMenu({ onBack }: MemoryMenuProps) {
  const [selectedIndex, setSelectedIndex] = useState(0);

  const menuItems = [
    { id: 'search', label: t('memory.search') },
    { id: 'ghost', label: t('memory.ghost') },
    { id: 'kanban', label: t('memory.kanban') },
    { id: 'back', label: t('daemon.back') },
  ];

  useInput((input, key) => {
    if (key.upArrow) {
      setSelectedIndex(prev => (prev > 0 ? prev - 1 : menuItems.length - 1));
    } else if (key.downArrow) {
      setSelectedIndex(prev => (prev < menuItems.length - 1 ? prev + 1 : 0));
    } else if (key.return) {
      if (menuItems[selectedIndex].id === 'back') {
        onBack();
      }
      // TODO: Implement memory actions
    } else if (input === 'q' || key.escape) {
      onBack();
    }
  });

  return (
    <Box flexDirection="column" padding={1}>
      <Box flexDirection="column" borderStyle="round" borderColor="cyan" paddingX={2} paddingY={1}>
        <Text bold>{t('memory.title')}</Text>
        
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
