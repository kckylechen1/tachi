import { writeFileSync, readFileSync, existsSync, mkdirSync } from 'fs';
import { join } from 'path';
import { homedir } from 'os';
import yaml from 'js-yaml';

export interface Config {
  daemon: {
    port: number;
    autoStart: boolean;
  };
  ui: {
    language: 'en' | 'zh';
    theme: 'dark' | 'light';
  };
  paths: {
    dataDir: string;
    projectDb?: string;
  };
}

const defaultConfig: Config = {
  daemon: {
    port: 6919,
    autoStart: true,
  },
  ui: {
    language: 'en',
    theme: 'dark',
  },
  paths: {
    dataDir: join(homedir(), '.tachi'),
  },
};

const configDir = join(homedir(), '.tachi');
const configPath = join(configDir, 'config.yaml');

export async function initConfig(): Promise<void> {
  if (!existsSync(configDir)) {
    mkdirSync(configDir, { recursive: true });
  }
  
  if (!existsSync(configPath)) {
    saveConfig(defaultConfig);
  }
}

export function loadConfig(): Config {
  try {
    const content = readFileSync(configPath, 'utf-8');
    const parsed = yaml.load(content) as Partial<Config>;
    return { ...defaultConfig, ...parsed };
  } catch {
    return defaultConfig;
  }
}

export function saveConfig(config: Config): void {
  const yamlContent = yaml.dump(config);
  writeFileSync(configPath, yamlContent, 'utf-8');
}

export function getDataDir(): string {
  const config = loadConfig();
  return config.paths.dataDir;
}

export function getBinaryPath(): string {
  const dataDir = getDataDir();
  return join(dataDir, 'bin', 'tachi-daemon');
}