import React from 'react';
import { SkillsManager } from './SkillsManager.js';

interface SkillsMenuProps {
  onBack: () => void;
}

export function SkillsMenu({ onBack }: SkillsMenuProps) {
  return <SkillsManager onBack={onBack} />;
}
