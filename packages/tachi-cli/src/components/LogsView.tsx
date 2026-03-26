import React, { useState, useEffect } from 'react';
import { Box, Text, useInput, useApp } from 'ink';
import { t } from '../utils/i18n.js';
import { colors } from '../utils/ui.js';
import { readFileSync, existsSync } from 'fs';
import { join } from 'path';
import { getDataDir } from '../utils/config.js';

interface LogsViewProps {
  onBack: () => void;
}

export function LogsView({ onBack }: LogsViewProps) {
  const [logs, setLogs] = useState<string[]>([]);
  const [scrollIndex, setScrollIndex] = useState(0);
  const [loading, setLoading] = useState(true);
  const { exit } = useApp();

  useEffect(() => {
    loadLogs();
  }, []);

  const loadLogs = () => {
    try {
      const dataDir = getDataDir();
      const logPath = join(dataDir, 'daemon.log');
      
      if (existsSync(logPath)) {
        const content = readFileSync(logPath, 'utf-8');
        const lines = content.split('\n').filter(line => line.trim());
        // Show last 50 lines
        setLogs(lines.slice(-50));
        setScrollIndex(Math.max(0, lines.length - 50));
      } else {
        setLogs(['No log file found. Start the daemon first.']);
      }
    } catch (error) {
      setLogs([`Error reading logs: ${error}`]);
    }
    setLoading(false);
  };

  useInput((input, key) => {
    if (key.upArrow) {
      setScrollIndex(prev => Math.max(0, prev - 1));
    } else if (key.downArrow) {
      setScrollIndex(prev => Math.min(logs.length - 1, prev + 1));
    } else if (input === 'r') {
      // Refresh logs
      loadLogs();
    } else if (input === 'q' || key.escape) {
      onBack();
    }
  });

  const visibleLogs = logs.slice(scrollIndex, scrollIndex + 15);

  return (
    <Box flexDirection="column" padding={1}>
      <Box flexDirection="column" borderStyle="round" borderColor="cyan" paddingX={1} paddingY={1}>
        <Text bold>{t('daemon.logs')}</Text>
        <Text dimColor>Showing last {logs.length} lines (r=refresh, q=back)</Text>
        
        <Box flexDirection="column" marginTop={1}>
          {loading ? (
            <Text dimColor>Loading...</Text>
          ) : (
            visibleLogs.map((log, index) => (
              <Text key={index} dimColor>{log}</Text>
            ))
          )}
        </Box>
        
        {!loading && logs.length > 15 && (
          <Box marginTop={1}>
            <Text dimColor>
              Line {scrollIndex + 1}-{Math.min(scrollIndex + 15, logs.length)} of {logs.length}
            </Text>
          </Box>
        )}
      </Box>

      <Box marginTop={1}>
        <Text dimColor>↑↓ Scroll | r Refresh | q Back</Text>
      </Box>
    </Box>
  );
}