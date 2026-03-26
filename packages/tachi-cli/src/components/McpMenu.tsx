import React from 'react';
import { McpManager } from './McpManager.js';

interface McpMenuProps {
  onBack: () => void;
}

export function McpMenu({ onBack }: McpMenuProps) {
  return <McpManager onBack={onBack} />;
}
