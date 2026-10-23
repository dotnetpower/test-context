export type JsonPrimitive = string | number | boolean | null;
export type JsonValue = JsonPrimitive | JsonValue[] | { [key: string]: JsonValue };

export interface DaemonRequest {
  root: string | null;
  name: string;
  arguments: { [key: string]: JsonValue };
  compact: boolean;
}

export interface WorkspaceSpec {
  root: string;
  scopes: string[];
  globs: string[];
  excludes?: string[];
  default_excludes?: boolean;
  pytest_cmd?: string[];
  allow_worktrees?: boolean;
  managed_parent?: string | null;
}

export interface DisposableLike {
  dispose(): void;
}

export interface CancellationLike {
  readonly isCancellationRequested: boolean;
  onCancellationRequested(listener: () => void): DisposableLike;
}

export interface ClientSettings {
  socketPath: string;
  autoStartService: boolean;
  requestTimeoutMs: number;
  wslDistribution?: string;
  wslExecutablePath?: string;
}
