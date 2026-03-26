import React, { useState, useEffect } from 'react';
import { Box, Text, useApp } from 'ink';
import { stopDaemon } from '../utils/daemon.js';
import { colors } from '../utils/ui.js';

export function StopCommand() {
  const { exit } = useApp();
  const [status, setStatus] = useState<'loading' | 'success' | 'error'>('loading');
  const [error, setError] = useState('');

  useEffect(() => {
    stopDaemon().then(result => {
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
        <Text>{colors.dim('Stopping daemon...')}</Text>
      )}
      {status === 'success' && (
        <Text>{colors.success('✓ Daemon stopped successfully')}</Text>
      )}
      {status === 'error' && (
        <Box flexDirection="column">
          <Text>{colors.error('✗ Failed to stop daemon')}</Text>
          <Text>{colors.dim(error)}</Text>
        </Box>
      )}
    </Box>
  );
}