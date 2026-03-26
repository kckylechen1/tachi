import React, { useState } from 'react';
import { Box, Text, useInput } from 'ink';
import { t } from '../utils/i18n.js';
import { colors } from '../utils/ui.js';
import { addMcpServer } from '../utils/mcp.js';

interface McpAddFormProps {
  onBack: () => void;
}

type FormField = 'name' | 'command' | 'args' | 'confirm';

export function McpAddForm({ onBack }: McpAddFormProps) {
  const [field, setField] = useState<FormField>('name');
  const [name, setName] = useState('');
  const [command, setCommand] = useState('npx');
  const [args, setArgs] = useState('-y @modelcontextprotocol/server-filesystem');
  const [error, setError] = useState('');
  const [success, setSuccess] = useState(false);

  useInput((input, key) => {
    if (success) {
      if (key.return) {
        onBack();
      }
      return;
    }

    if (key.return) {
      if (field === 'name' && name.trim()) {
        setField('command');
      } else if (field === 'command' && command.trim()) {
        setField('args');
      } else if (field === 'args') {
        setField('confirm');
      } else if (field === 'confirm') {
        handleSubmit();
      }
    } else if (key.escape) {
      onBack();
    } else if (field === 'name') {
      if (key.backspace || key.delete) {
        setName(prev => prev.slice(0, -1));
      } else if (input) {
        setName(prev => prev + input);
      }
    } else if (field === 'command') {
      if (key.backspace || key.delete) {
        setCommand(prev => prev.slice(0, -1));
      } else if (input) {
        setCommand(prev => prev + input);
      }
    } else if (field === 'args') {
      if (key.backspace || key.delete) {
        setArgs(prev => prev.slice(0, -1));
      } else if (input) {
        setArgs(prev => prev + input);
      }
    } else if (field === 'confirm') {
      if (input === 'y' || input === 'Y') {
        handleSubmit();
      } else if (input === 'n' || input === 'N') {
        onBack();
      }
    }
  });

  const handleSubmit = () => {
    try {
      addMcpServer({
        name: name.trim(),
        command: command.trim(),
        args: args.trim().split(/\s+/),
        enabled: true,
      });
      setSuccess(true);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Unknown error');
    }
  };

  if (success) {
    return (
      <Box flexDirection="column" padding={1}>
        <Box borderStyle="round" borderColor="green" paddingX={2} paddingY={1}>
          <Text>{colors.success('✓ MCP server added successfully!')}</Text>
          <Box marginTop={1}>
            <Text dimColor>Press Enter to go back</Text>
          </Box>
        </Box>
      </Box>
    );
  }

  return (
    <Box flexDirection="column" padding={1}>
      <Box flexDirection="column" borderStyle="round" borderColor="cyan" paddingX={2} paddingY={1}>
        <Text bold>{t('mcp.add')}</Text>

        {error && (
          <Box marginTop={1}>
            <Text>{colors.error(`✗ ${error}`)}</Text>
          </Box>
        )}

        <Box flexDirection="column" marginTop={1}>
          <Box>
            <Text>
              {field === 'name' ? colors.primary('❯ ') : '  '}
              Name: {name}
              {field === 'name' && <Text color="cyan">▌</Text>}
            </Text>
          </Box>

          <Box>
            <Text>
              {field === 'command' ? colors.primary('❯ ') : '  '}
              Command: {command}
              {field === 'command' && <Text color="cyan">▌</Text>}
            </Text>
          </Box>

          <Box>
            <Text>
              {field === 'args' ? colors.primary('❯ ') : '  '}
              Args: {args}
              {field === 'args' && <Text color="cyan">▌</Text>}
            </Text>
          </Box>

          {field === 'confirm' && (
            <Box marginTop={1}>
              <Text>{colors.warning('Add this MCP server? (y/n)')}</Text>
            </Box>
          )}
        </Box>

        <Box flexDirection="column" marginTop={2}>
          <Text dimColor>Enter - Next field | Esc - Cancel</Text>
        </Box>
      </Box>
    </Box>
  );
}
