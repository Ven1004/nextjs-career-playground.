import type { IncomingMessage, ServerResponse } from 'http'
import type { UrlObject } from 'url'
import type { Duplex } from 'stream'
import type { webpack } from 'next/dist/compiled/webpack/webpack'
import type getBaseWebpackConfig from '../../build/webpack-config'
import type { RouteDefinition } from '../route-definitions/route-definition'
import type { Project, Update as TurbopackUpdate } from '../../build/swc/types'
import type { VersionInfo } from './parse-version-info'
import type { DebugInfo } from '../../next-devtools/shared/types'
import type { DevIndicatorServerState } from './dev-indicator-server-state'

export const enum HMR_ACTIONS_SENT_TO_BROWSER {
  ADDED_PAGE = 'addedPage',
  REMOVED_PAGE = 'removedPage',
  RELOAD_PAGE = 'reloadPage',
  SERVER_COMPONENT_CHANGES = 'serverComponentChanges',
  MIDDLEWARE_CHANGES = 'middlewareChanges',
  CLIENT_CHANGES = 'clientChanges',
  SERVER_ONLY_CHANGES = 'serverOnlyChanges',
  SYNC = 'sync',
  BUILT = 'built',
  BUILDING = 'building',
  DEV_PAGES_MANIFEST_UPDATE = 'devPagesManifestUpdate',
  TURBOPACK_MESSAGE = 'turbopack-message',
  SERVER_ERROR = 'serverError',
  TURBOPACK_CONNECTED = 'turbopack-connected',
  ISR_MANIFEST = 'isrManifest',
  DEV_INDICATOR = 'devIndicator',
}

interface ServerErrorAction {
  action: HMR_ACTIONS_SENT_TO_BROWSER.SERVER_ERROR
  errorJSON: string
}

export interface TurbopackMessageAction {
  action: HMR_ACTIONS_SENT_TO_BROWSER.TURBOPACK_MESSAGE
  data: TurbopackUpdate | TurbopackUpdate[]
}

interface BuildingAction {
  action: HMR_ACTIONS_SENT_TO_BROWSER.BUILDING
}

export interface CompilationError {
  moduleName?: string
  message: string
  details?: string
  moduleTrace?: Array<{ moduleName?: string }>
  stack?: string
}
export interface SyncAction {
  action: HMR_ACTIONS_SENT_TO_BROWSER.SYNC
  hash: string
  errors: ReadonlyArray<CompilationError>
  warnings: ReadonlyArray<CompilationError>
  versionInfo: VersionInfo
  updatedModules?: ReadonlyArray<string>
  debug?: DebugInfo
  devIndicator: DevIndicatorServerState
}
interface BuiltAction {
  action: HMR_ACTIONS_SENT_TO_BROWSER.BUILT
  hash: string
  errors: ReadonlyArray<CompilationError>
  warnings: ReadonlyArray<CompilationError>
  updatedModules?: ReadonlyArray<string>
}

interface AddedPageAction {
  action: HMR_ACTIONS_SENT_TO_BROWSER.ADDED_PAGE
  data: [page: string | null]
}

interface RemovedPageAction {
  action: HMR_ACTIONS_SENT_TO_BROWSER.REMOVED_PAGE
  data: [page: string | null]
}

export interface ReloadPageAction {
  action: HMR_ACTIONS_SENT_TO_BROWSER.RELOAD_PAGE
  data: string
}

interface ServerComponentChangesAction {
  action: HMR_ACTIONS_SENT_TO_BROWSER.SERVER_COMPONENT_CHANGES
  hash: string
}

interface MiddlewareChangesAction {
  event: HMR_ACTIONS_SENT_TO_BROWSER.MIDDLEWARE_CHANGES
}

interface ClientChangesAction {
  event: HMR_ACTIONS_SENT_TO_BROWSER.CLIENT_CHANGES
}

interface ServerOnlyChangesAction {
  event: HMR_ACTIONS_SENT_TO_BROWSER.SERVER_ONLY_CHANGES
  pages: ReadonlyArray<string>
}

interface DevPagesManifestUpdateAction {
  action: HMR_ACTIONS_SENT_TO_BROWSER.DEV_PAGES_MANIFEST_UPDATE
  data: [
    {
      devPagesManifest: true
    },
  ]
}

export interface TurbopackConnectedAction {
  action: HMR_ACTIONS_SENT_TO_BROWSER.TURBOPACK_CONNECTED
  data: { sessionId: number }
}

export interface AppIsrManifestAction {
  action: HMR_ACTIONS_SENT_TO_BROWSER.ISR_MANIFEST
  data: Record<string, boolean>
}

export interface DevIndicatorAction {
  action: HMR_ACTIONS_SENT_TO_BROWSER.DEV_INDICATOR
  devIndicator: DevIndicatorServerState
}

export type HMR_ACTION_TYPES =
  | TurbopackMessageAction
  | TurbopackConnectedAction
  | BuildingAction
  | SyncAction
  | BuiltAction
  | AddedPageAction
  | RemovedPageAction
  | ReloadPageAction
  | ServerComponentChangesAction
  | ClientChangesAction
  | MiddlewareChangesAction
  | ServerOnlyChangesAction
  | DevPagesManifestUpdateAction
  | ServerErrorAction
  | AppIsrManifestAction
  | DevIndicatorAction

export type TurbopackMsgToBrowser =
  | { type: HMR_ACTIONS_SENT_TO_BROWSER.TURBOPACK_MESSAGE; data: any }
  | {
      type: HMR_ACTIONS_SENT_TO_BROWSER.TURBOPACK_CONNECTED
      data: { sessionId: number }
    }

export interface NextJsHotReloaderInterface {
  turbopackProject?: Project
  activeWebpackConfigs?: Array<Awaited<ReturnType<typeof getBaseWebpackConfig>>>
  serverStats: webpack.Stats | null
  edgeServerStats: webpack.Stats | null
  run(
    req: IncomingMessage,
    res: ServerResponse,
    parsedUrl: UrlObject
  ): Promise<{ finished?: true }>

  setHmrServerError(error: Error | null): void
  clearHmrServerError(): void
  start(): Promise<void>
  send(action: HMR_ACTION_TYPES): void
  getCompilationErrors(page: string): Promise<any[]>
  onHMR(
    req: IncomingMessage,
    _socket: Duplex,
    head: Buffer,
    onUpgrade: (client: { send(data: string): void }) => void
  ): void
  invalidate({
    reloadAfterInvalidation,
  }: {
    reloadAfterInvalidation: boolean
  }): Promise<void> | void
  buildFallbackError(): Promise<void>
  ensurePage({
    page,
    clientOnly,
    appPaths,
    definition,
    isApp,
    url,
  }: {
    page: string
    clientOnly: boolean
    appPaths?: ReadonlyArray<string> | null
    isApp?: boolean
    definition: RouteDefinition | undefined
    url?: string
  }): Promise<void>
  close(): void
}
