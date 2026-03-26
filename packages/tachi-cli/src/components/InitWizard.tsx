import React, { useState } from 'react';
import { Box, Text, useInput, useApp } from 'ink';
import { banner, colors } from '../utils/ui.js';
import { t, setLanguage, type Language } from '../utils/i18n.js';
import { saveConfig, loadConfig } from '../utils/config.js';
import { startDaemon } from '../utils/daemon.js';

type Step = 'language' | 'complete';

export function InitWizard() {
  const { exit } = useApp();
  const [step, setStep] = useState<Step>('language');
  const [selectedLang, setSelectedLang] = useState<Language>('en');
  const [selectedIndex, setSelectedIndex] = useState(0);

  const languages = [
    { code: 'en' as Language, label: 'English' },
    { code: 'zh' as Language, label: '中文' },
  ];

  useInput((input, key) => {
    if (step === 'language') {
      if (key.upArrow) {
        setSelectedIndex(prev => (prev > 0 ? prev - 1 : languages.length - 1));
      } else if (key.downArrow) {
        setSelectedIndex(prev => (prev < languages.length - 1 ? prev + 1 : 0));
      } else if (key.return) {
        const lang = languages[selectedIndex].code;
        setSelectedLang(lang);
        setLanguage(lang);
        
        // Save config
        const config = loadConfig();
        config.ui.language = lang;
        saveConfig(config);
        
        setStep('complete');
        
        // Auto-start daemon
        setTimeout(async () => {
          await startDaemon();
          exit();
        }, 2000);
      } else if (key.escape || input === 'q') {
        exit();
      }
    }
  });

  return (
    <Box flexDirection="column" padding={1}>
      <Text>{banner}</Text>
      
      {step === 'language' && (
        <Box flexDirection="column" marginTop={1}>
          <Box borderStyle="round" borderColor="cyan" paddingX={2} paddingY={1}>
            <Text bold>{t('init.welcome')}</Text>
            <Text dimColor>{t('init.setup')}</Text>
            
            <Box flexDirection="column" marginTop={1}>
              <Text>{t('settings.language')}:</Text>
              {languages.map((lang, index) => (
                <Text key={lang.code}>
                  {index === selectedIndex ? colors.primary('❯ ') : '  '}
                  {index === selectedIndex ? colors.primary(lang.label) : lang.label}
                  {lang.code === selectedLang && colors.success(' ✓')}
                </Text>
              ))}
            </Box>
          </Box>
          
          <Box marginTop={1}>
            <Text dimColor>↑↓ Navigate | Enter Confirm | q Quit</Text>
          </Box>
        </Box>
      )}
      
      {step === 'complete' && (
        <Box flexDirection="column" marginTop={1}>
          <Box borderStyle="round" borderColor="green" paddingX={2} paddingY={1}>
            <Text>{colors.success('✓ ' + t('init.complete'))}</Text>
            <Box marginTop={1}>
              <Text dimColor>{t('init.startNow')}...</Text>
            </Box>
          </Box>
        </Box>
      )}
    </Box>
  );
}