import React from 'react';
import { MemoryManager } from './MemoryManager.js';

interface MemoryMenuProps {
  onBack: () => void;
}

export function MemoryMenu({ onBack }: MemoryMenuProps) {
  return <MemoryManager onBack={onBack} />;
}
