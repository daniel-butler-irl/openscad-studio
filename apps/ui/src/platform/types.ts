/**
 * Platform abstraction types for cross-platform support.
 * Enables the app to run as both a Tauri desktop app and a pure web app.
 */

export type ExportFormat = 'stl' | 'obj' | 'amf' | '3mf' | 'png' | 'svg' | 'dxf';

export interface FileOpenResult {
  /** File path on disk (null for web where no real path exists) */
  path: string | null;
  /** Display name for the file */
  name: string;
  /** File contents as text */
  content: string;
}

export interface PlatformCapabilities {
  /** Whether the platform supports multiple open files/tabs (Tauri=true, Web=false) */
  multiFile: boolean;
  /** Whether the platform has a native OS menu bar (Tauri=true, Web=false) */
  hasNativeMenu: boolean;
  /** Whether the platform has full filesystem access (Tauri=true, Web=limited) */
  hasFileSystem: boolean;
  /** Whether the platform can set the window title (both can) */
  canSetWindowTitle: boolean;
  /** Whether the subscription bridge is installed and backed by native secure storage. */
  hasSubscriptionAuth: boolean;
}

/** Providers whose credentials are entered as API keys in the app. */
export type ApiKeyProvider = 'anthropic' | 'openai' | 'openai-compatible';

/** Providers authenticated through a desktop subscription account. */
export type SubscriptionProvider = 'codex-subscription' | 'grok-subscription';

export type AiConnectionProvider = ApiKeyProvider | SubscriptionProvider;

/** Deliberately excludes access tokens, refresh tokens, and provider response bodies. */
export interface SubscriptionAccountStatus {
  provider: SubscriptionProvider;
  state: 'signed-out' | 'pending' | 'signed-in' | 'error';
  accountId: string | null;
  generation: number;
  message?: string;
}

export type SubscriptionLoginStart =
  | {
      kind: 'browser-pkce';
      loginId: string;
      verificationUrl: string;
      /** Empty for browser authorization. */
      userCode: string;
      expiresAt: number;
    }
  | {
      kind: 'device-code';
      loginId: string;
      verificationUrl: string;
      userCode: string;
      expiresAt: number;
    };

/** Challenge expiry uses epoch milliseconds. OAuth secrets stay native-only. */

export interface SubscriptionModelInfo {
  id: string;
  name: string;
  apiBackend: 'chat-completions' | 'responses' | 'unknown';
  /** `unknown` remains distinct from an explicit lack of support. */
  images: 'supported' | 'unsupported' | 'unknown';
  reasoning: 'supported' | 'unsupported' | 'unknown';
  tools: 'supported' | 'unsupported' | 'unknown';
  recommended: boolean;
  contextWindow?: number;
}

export interface SubscriptionRequest {
  requestId: string;
  provider: SubscriptionProvider;
  accountGeneration: number;
  modelId: string;
  /** Native validates this payload and sets model, stream, and provider-specific safety fields. */
  body: Record<string, unknown>;
  /** Native-issued and scoped to the provider, account generation, and originating window. */
  continuationId?: string;
}

export type SubscriptionStreamEvent =
  | {
      requestId: string;
      sequence: number;
      kind: 'response';
      status: number;
      headers: Record<string, string>;
    }
  | { requestId: string; sequence: number; kind: 'chunk'; bytes: number[] }
  | { requestId: string; sequence: number; kind: 'complete' }
  | { requestId: string; sequence: number; kind: 'error'; message: string };

/** Native implementation owns fixed provider origins and all authorization headers. */
export interface SubscriptionBridge {
  getStatus(provider: SubscriptionProvider): Promise<SubscriptionAccountStatus>;
  startLogin(provider: SubscriptionProvider): Promise<SubscriptionLoginStart>;
  cancelLogin(provider: SubscriptionProvider, loginId: string): Promise<void>;
  signOut(provider: SubscriptionProvider): Promise<void>;
  listModels(
    provider: SubscriptionProvider,
    accountGeneration: number
  ): Promise<SubscriptionModelInfo[]>;
  startRequest(
    request: SubscriptionRequest,
    onEvent: (event: SubscriptionStreamEvent) => void
  ): Promise<void>;
  cancelRequest(requestId: string): Promise<void>;
}

export interface ConfirmDialogOptions {
  title?: string;
  kind?: 'info' | 'warning' | 'error';
  okLabel?: string;
  cancelLabel?: string;
}

export interface FileFilter {
  name: string;
  extensions: string[];
}

export interface LanAccessStatus {
  running: boolean;
  urls: string[];
  message: string | null;
}

export interface PlatformBridge {
  getLanAccessStatus(): Promise<LanAccessStatus>;
  setLanAccess(enabled: boolean): Promise<LanAccessStatus>;
  getLanCertificate(): Promise<string>;

  readonly capabilities: PlatformCapabilities;
  /** Present only in the desktop bridge; never implemented with browser storage. */
  readonly subscriptions?: SubscriptionBridge;

  // -- File operations --

  /** Open a file. Returns null if user cancelled. */
  fileOpen(filters?: FileFilter[]): Promise<FileOpenResult | null>;

  /** Read a file by path. Only works on platforms with full filesystem access. */
  fileRead(path: string): Promise<FileOpenResult | null>;

  /**
   * Save content to a file.
   * If path is provided, saves directly (Tauri only). Otherwise prompts.
   * Returns the saved path, or null if cancelled.
   */
  fileSave(
    content: string,
    path?: string | null,
    filters?: FileFilter[],
    defaultFilename?: string
  ): Promise<string | null>;

  /** Always prompts for a new save location. Returns the saved path, or null if cancelled. */
  fileSaveAs(
    content: string,
    filters?: FileFilter[],
    defaultFilename?: string
  ): Promise<string | null>;

  /** Export binary data (e.g., STL, PNG). Prompts for save location on desktop, triggers download on web. */
  fileExport(data: Uint8Array, defaultFilename: string, filters?: FileFilter[]): Promise<void>;

  // -- Dialogs --

  /** Confirmation dialog (OK/Cancel). Returns true if confirmed. */
  confirm(message: string, options?: ConfirmDialogOptions): Promise<boolean>;

  /** Ask dialog with Save/Don't Save semantics. Returns true if user chose the affirmative action. */
  ask(message: string, options?: ConfirmDialogOptions): Promise<boolean>;

  // -- Window --

  /** Set the window/document title */
  setWindowTitle(title: string): void;

  /**
   * Register a handler for window close requests.
   * Handler should return true to allow close, false to prevent it.
   * Returns an unsubscribe function.
   */
  onCloseRequested(handler: () => Promise<boolean>): () => void;

  // -- Directory --

  /**
   * Check if a file exists at the given absolute path.
   * Web bridge always returns false.
   */
  fileExists(absolutePath: string): Promise<boolean>;

  /**
   * Read a single text file by absolute path.
   * Returns the file contents, or null if the file doesn't exist or can't be read.
   * Web bridge always returns null.
   */
  readTextFile(absolutePath: string): Promise<string | null>;

  /**
   * Read OpenSCAD project files in a directory.
   * Returns a map of relative paths to file contents.
   * Used to populate the WASM virtual filesystem for include/use resolution.
   * @param recursive - If true (default), recursively walk subdirectories.
   *   Use false for working directory reads (only need sibling files).
   */
  readDirectoryFiles(
    dirPath: string,
    extensions?: string[],
    recursive?: boolean
  ): Promise<Record<string, string>>;

  /**
   * Get well-known OS library paths that exist on disk.
   * Returns paths like ~/Documents/OpenSCAD/libraries/ on macOS.
   * Web bridge returns empty array (no filesystem access).
   */
  getLibraryPaths(): Promise<string[]>;

  /**
   * Open a directory picker dialog. Returns the selected path or null if cancelled.
   * Web bridge returns null (no filesystem access).
   */
  pickDirectory(): Promise<string | null>;

  // -- File CRUD (for project file management) --

  /** Write a text file to the given absolute path. Web bridge is a no-op. */
  writeTextFile(absolutePath: string, content: string): Promise<void>;

  /** Delete a file at the given absolute path. Web bridge is a no-op. */
  deleteFile(absolutePath: string): Promise<void>;

  /** Rename/move a file. Web bridge is a no-op. */
  renameFile(oldPath: string, newPath: string): Promise<void>;

  /**
   * List all subdirectory paths (relative) under a directory, recursively.
   * Used to discover empty folders on project open.
   * Web bridge returns empty array.
   */
  readSubdirectories(dirPath: string): Promise<string[]>;

  /** Create a directory (and any missing parents). Web bridge is a no-op. */
  createDirectory(absolutePath: string): Promise<void>;

  /** Remove a directory and all its contents recursively. Web bridge is a no-op. */
  removeDirectory(absolutePath: string): Promise<void>;

  // -- File watching --

  /**
   * Watch a directory for changes to OpenSCAD project files.
   * Callback receives the relative path of the changed file and its new content.
   * Returns an unsubscribe function. Web bridge is a no-op (returns no-op unsub).
   */
  watchDirectory(
    dirPath: string,
    onChange: (relativePath: string, content: string | null) => void
  ): Promise<() => void>;

  // -- Project directory --

  /**
   * Get the platform default base directory for new projects.
   * Desktop: ~/Documents/OpenSCAD Studio/
   * Web: returns null (no filesystem).
   */
  getDefaultProjectsDirectory(): Promise<string | null>;

  /**
   * Create a new project directory inside basePath with the given name.
   * Deduplicates by appending -2, -3, etc. if the name already exists.
   * Returns the absolute path of the created directory, or null on failure.
   * Web: returns null.
   */
  createProjectDirectory(basePath: string, name: string): Promise<string | null>;

  // -- Lifecycle --

  /** Optional initialization (e.g., setting up native menu event forwarding) */
  initialize?(): Promise<void>;
}
