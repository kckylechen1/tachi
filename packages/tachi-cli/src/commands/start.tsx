import React, { useState, useEffect } from 'react';
import { Box, Text, useApp } from 'ink';
import { startDaemon } from '../utils/daemon.js';
import { t } from '../utils/i18n.js';
import { colors } from '../utils/ui.js';

export function StartCommand() {
  const { exit } = useApp();
  const [status, setStatus] = useState<'loading' | 'success' | 'error'>('loading');
  const [error, setError] = useState('');

  useEffect(() => {
    startDaemon().then(result => {
      if (result.success) {
        setStatus('success');
        setTimeout(() => exit(), 1500);
      } else {
        setStatus('error');
        setError(result.error || 'Unknown error');
        setTimeout(() => exit(), 2000);
      }
    });
  }, []);

  return (
    <Box flexDirection="column" padding={1}>
      {status === 'loading' && (
        <Text>{colors.dim('Starting daemon...')}</Text>
      )}
      {status === 'success' && (
        <Text>{colors.success('✓ Daemon started successfully')}</Text>
      )}
      {status === 'error' && (
        <Box flexDirection="column">
          <Text>{colors.error('✗ Failed to start daemon')}</Text>
          <Text>{colors.dim(error)}</Text>
        </Box>
      )}
    </Box>
  );
}