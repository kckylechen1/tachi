import React, { useState, useEffect } from 'react';
import { Box, Text, useApp } from 'ink';
import { getDaemonStatus } from '../utils/daemon.js';
import { t } from '../utils/i18n.js';
import { colors } from '../utils/ui.js';

export function StatusCommand() {
  const { exit } = useApp();
  const [status, setStatus] = useState<{ online: boolean; port?: number; memoryCount?: number }>({ online: false });
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    getDaemonStatus().then(result => {
      setStatus(result);
      setLoading(false);
      setTimeout(() => exit(), loading ? 1000 : 0);
    });
  }, []);

  if (loading) {
    return (
      <Box padding={1}>
        <Text>{colors.dim('Checking status...')}</Text>
      </Box>
    );
  }

  return (
    <Box flexDirection="column" padding={1}>
      <Box flexDirection="column" borderStyle="round" borderColor={status.online ? 'green' : 'red'} paddingX={2} paddingY={1}>
        <Text bold>{t('daemon.status')}</Text>
        <Box flexDirection="column" marginTop={1}>
          <Text>
            {status.online ? colors.success(`🟢 ${t('daemon.online')}`) : colors.error(`🔴 ${t('daemon.offline')}`)}
          </Text>
          {status.online && status.port && (
            <Text>{colors.dim(`${t('daemon.port')}: ${status.port}`)}</Text>
          )}
          {status.online && status.memoryCount !== undefined && (
            <Text>{colors.dim(`${t('daemon.memory')}: ${status.memoryCount}`)}</Text>
          )}
        </Box>
      </Box>
    </Box>
  );
}