import type { IncomingHttpHeaders, OutgoingHttpHeaders } from 'node:http'
import type { SizeLimit } from '../../types'
import type { RequestStore } from '../app-render/work-unit-async-storage.external'
import type { AppRenderContext, GenerateFlight } from './app-render'
import type { AppPageModule } from '../route-modules/app-page/module'
import type { BaseNextRequest, BaseNextResponse } from '../base-http'

import {
  RSC_HEADER,
  RSC_CONTENT_TYPE_HEADER,
  NEXT_ROUTER_STATE_TREE_HEADER,
  ACTION_HEADER,
} from '../../client/components/app-router-headers'
import {
  getAccessFallbackHTTPStatus,
  isHTTPAccessFallbackError,
} from '../../client/components/http-access-fallback/http-access-fallback'
import {
  getRedirectTypeFromError,
  getURLFromRedirectError,
} from '../../client/components/redirect'
import {
  isRedirectError,
  type RedirectType,
} from '../../client/components/redirect-error'
import RenderResult, {
  type AppPageRenderResultMetadata,
} from '../render-result'
import type { WorkStore } from '../app-render/work-async-storage.external'
import { FlightRenderResult } from './flight-render-result'
import {
  filterReqHeaders,
  actionsForbiddenHeaders,
} from '../lib/server-ipc/utils'
import { getModifiedCookieValues } from '../web/spec-extension/adapters/request-cookies'

import {
  NEXT_CACHE_REVALIDATED_TAGS_HEADER,
  NEXT_CACHE_REVALIDATE_TAG_TOKEN_HEADER,
} from '../../lib/constants'
import { getServerActionRequestMetadata } from '../lib/server-action-request-meta'
import { isCsrfOriginAllowed } from './csrf-protection'
import { warn } from '../../build/output/log'
import { RequestCookies, ResponseCookies } from '../web/spec-extension/cookies'
import { HeadersAdapter } from '../web/spec-extension/adapters/headers'
import { fromNodeOutgoingHttpHeaders } from '../web/utils'
import { selectWorkerForForwarding } from './action-utils'
import { isNodeNextRequest, isWebNextRequest } from '../base-http/helpers'
import { RedirectStatusCode } from '../../client/components/redirect-status-code'
import { synchronizeMutableCookies } from '../async-storage/request-store'
import type { TemporaryReferenceSet } from 'react-server-dom-webpack/server.edge'
import { workUnitAsyncStorage } from '../app-render/work-unit-async-storage.external'
import { executeRevalidates } from '../revalidation-utils'
import { getRequestMeta } from '../request-meta'

function formDataFromSearchQueryString(query: string) {
  const searchParams = new URLSearchParams(query)
  const formData = new FormData()
  for (const [key, value] of searchParams) {
    formData.append(key, value)
  }
  return formData
}

function nodeHeadersToRecord(
  headers: IncomingHttpHeaders | OutgoingHttpHeaders
) {
  const record: Record<string, string> = {}
  for (const [key, value] of Object.entries(headers)) {
    if (value !== undefined) {
      record[key] = Array.isArray(value) ? value.join(', ') : `${value}`
    }
  }
  return record
}

function getForwardedHeaders(
  req: BaseNextRequest,
  res: BaseNextResponse
): Headers {
  // Get request headers and cookies
  const requestHeaders = req.headers
  const requestCookies = new RequestCookies(HeadersAdapter.from(requestHeaders))

  // Get response headers and cookies
  const responseHeaders = res.getHeaders()
  const responseCookies = new ResponseCookies(
    fromNodeOutgoingHttpHeaders(responseHeaders)
  )

  // Merge request and response headers
  const mergedHeaders = filterReqHeaders(
    {
      ...nodeHeadersToRecord(requestHeaders),
      ...nodeHeadersToRecord(responseHeaders),
    },
    actionsForbiddenHeaders
  ) as Record<string, string>

  // Merge cookies into requestCookies, so responseCookies always take precedence
  // and overwrite/delete those from requestCookies.
  responseCookies.getAll().forEach((cookie) => {
    if (typeof cookie.value === 'undefined') {
      requestCookies.delete(cookie.name)
    } else {
      requestCookies.set(cookie)
    }
  })

  // Update the 'cookie' header with the merged cookies
  mergedHeaders['cookie'] = requestCookies.toString()

  // Remove headers that should not be forwarded
  delete mergedHeaders['transfer-encoding']

  return new Headers(mergedHeaders)
}

function addRevalidationHeader(
  res: BaseNextResponse,
  {
    workStore,
    requestStore,
  }: {
    workStore: WorkStore
    requestStore: RequestStore
  }
) {
  // If a tag was revalidated, the client router needs to invalidate all the
  // client router cache as they may be stale. And if a path was revalidated, the
  // client needs to invalidate all subtrees below that path.

  // To keep the header size small, we use a tuple of
  // [[revalidatedPaths], isTagRevalidated ? 1 : 0, isCookieRevalidated ? 1 : 0]
  // instead of a JSON object.

  // TODO-APP: Currently the prefetch cache doesn't have subtree information,
  // so we need to invalidate the entire cache if a path was revalidated.
  // TODO-APP: Currently paths are treated as tags, so the second element of the tuple
  // is always empty.

  const isTagRevalidated = workStore.pendingRevalidatedTags?.length ? 1 : 0
  const isCookieRevalidated = getModifiedCookieValues(
    requestStore.mutableCookies
  ).length
    ? 1
    : 0

  res.setHeader(
    'x-action-revalidated',
    JSON.stringify([[], isTagRevalidated, isCookieRevalidated])
  )
}

/**
 * Forwards a server action request to a separate worker. Used when the requested action is not available in the current worker.
 */
async function createForwardedActionResponse(
  req: BaseNextRequest,
  res: BaseNextResponse,
  host: Host,
  workerPathname: string,
  basePath: string
) {
  if (!host) {
    throw new Error(
      'Invariant: Missing `host` header from a forwarded Server Actions request.'
    )
  }

  const forwardedHeaders = getForwardedHeaders(req, res)

  // indicate that this action request was forwarded from another worker
  // we use this to skip rendering the flight tree so that we don't update the UI
  // with the response from the forwarded worker
  forwardedHeaders.set('x-action-forwarded', '1')

  const proto =
    getRequestMeta(req, 'initProtocol')?.replace(/:+$/, '') || 'https'

  // For standalone or the serverful mode, use the internal origin directly
  // other than the host headers from the request.
  const origin = process.env.__NEXT_PRIVATE_ORIGIN || `${proto}://${host.value}`

  const fetchUrl = new URL(`${origin}${basePath}${workerPathname}`)

  try {
    let body: BodyInit | ReadableStream<Uint8Array> | undefined
    if (
      // The type check here ensures that `req` is correctly typed, and the
      // environment variable check provides dead code elimination.
      process.env.NEXT_RUNTIME === 'edge' &&
      isWebNextRequest(req)
    ) {
      if (!req.body) {
        throw new Error('Invariant: missing request body.')
      }

      body = req.body
    } else if (
      // The type check here ensures that `req` is correctly typed, and the
      // environment variable check provides dead code elimination.
      process.env.NEXT_RUNTIME !== 'edge' &&
      isNodeNextRequest(req)
    ) {
      body = req.stream()
    } else {
      throw new Error('Invariant: Unknown request type.')
    }

    // Forward the request to the new worker
    const response = await fetch(fetchUrl, {
      method: 'POST',
      body,
      duplex: 'half',
      headers: forwardedHeaders,
      redirect: 'manual',
      next: {
        // @ts-ignore
        internal: 1,
      },
    })

    if (
      response.headers.get('content-type')?.startsWith(RSC_CONTENT_TYPE_HEADER)
    ) {
      // copy the headers from the redirect response to the response we're sending
      for (const [key, value] of response.headers) {
        if (!actionsForbiddenHeaders.includes(key)) {
          res.setHeader(key, value)
        }
      }

      return new FlightRenderResult(response.body!)
    } else {
      // Since we aren't consuming the response body, we cancel it to avoid memory leaks
      response.body?.cancel()
    }
  } catch (err) {
    // we couldn't stream the forwarded response, so we'll just return an empty response
    console.error(`failed to forward action response`, err)
  }

  return RenderResult.fromStatic('{}')
}

/**
 * Returns the parsed redirect URL if we deem that it is hosted by us.
 *
 * We handle both relative and absolute redirect URLs.
 *
 * In case the redirect URL is not relative to the application we return `null`.
 */
function getAppRelativeRedirectUrl(
  basePath: string,
  host: Host,
  redirectUrl: string
): URL | null {
  if (redirectUrl.startsWith('/') || redirectUrl.startsWith('.')) {
    // Make sure we are appending the basePath to relative URLS
    return new URL(`${basePath}${redirectUrl}`, 'http://n')
  }

  const parsedRedirectUrl = new URL(redirectUrl)

  if (host?.value !== parsedRedirectUrl.host) {
    return null
  }

  // At this point the hosts are the same, just confirm we
  // are routing to a path underneath the `basePath`
  return parsedRedirectUrl.pathname.startsWith(basePath)
    ? parsedRedirectUrl
    : null
}

async function createRedirectRenderResult(
  req: BaseNextRequest,
  res: BaseNextResponse,
  originalHost: Host,
  redirectUrl: string,
  redirectType: RedirectType,
  basePath: string,
  workStore: WorkStore
) {
  res.setHeader('x-action-redirect', `${redirectUrl};${redirectType}`)

  // If we're redirecting to another route of this Next.js application, we'll
  // try to stream the response from the other worker path. When that works,
  // we can save an extra roundtrip and avoid a full page reload.
  // When the redirect URL starts with a `/` or is to the same host, under the
  // `basePath` we treat it as an app-relative redirect;
  const appRelativeRedirectUrl = getAppRelativeRedirectUrl(
    basePath,
    originalHost,
    redirectUrl
  )

  if (appRelativeRedirectUrl) {
    if (!originalHost) {
      throw new Error(
        'Invariant: Missing `host` header from a forwarded Server Actions request.'
      )
    }

    const forwardedHeaders = getForwardedHeaders(req, res)
    forwardedHeaders.set(RSC_HEADER, '1')

    const proto =
      getRequestMeta(req, 'initProtocol')?.replace(/:+$/, '') || 'https'

    // For standalone or the serverful mode, use the internal origin directly
    // other than the host headers from the request.
    const origin =
      process.env.__NEXT_PRIVATE_ORIGIN || `${proto}://${originalHost.value}`

    const fetchUrl = new URL(
      `${origin}${appRelativeRedirectUrl.pathname}${appRelativeRedirectUrl.search}`
    )

    if (workStore.pendingRevalidatedTags) {
      forwardedHeaders.set(
        NEXT_CACHE_REVALIDATED_TAGS_HEADER,
        workStore.pendingRevalidatedTags.join(',')
      )
      forwardedHeaders.set(
        NEXT_CACHE_REVALIDATE_TAG_TOKEN_HEADER,
        workStore.incrementalCache?.prerenderManifest?.preview?.previewModeId ||
          ''
      )
    }

    // Ensures that when the path was revalidated we don't return a partial response on redirects
    forwardedHeaders.delete(NEXT_ROUTER_STATE_TREE_HEADER)
    // When an action follows a redirect, it's no longer handling an action: it's just a normal RSC request
    // to the requested URL. We should remove the `next-action` header so that it's not treated as an action
    forwardedHeaders.delete(ACTION_HEADER)

    try {
      const response = await fetch(fetchUrl, {
        method: 'GET',
        headers: forwardedHeaders,
        next: {
          // @ts-ignore
          internal: 1,
        },
      })

      if (
        response.headers
          .get('content-type')
          ?.startsWith(RSC_CONTENT_TYPE_HEADER)
      ) {
        // copy the headers from the redirect response to the response we're sending
        for (const [key, value] of response.headers) {
          if (!actionsForbiddenHeaders.includes(key)) {
            res.setHeader(key, value)
          }
        }

        return new FlightRenderResult(response.body!)
      } else {
        // Since we aren't consuming the response body, we cancel it to avoid memory leaks
        response.body?.cancel()
      }
    } catch (err) {
      // we couldn't stream the redirect response, so we'll just do a normal redirect
      console.error(`failed to get redirect response`, err)
    }
  }

  return RenderResult.fromStatic('{}')
}

// Used to compare Host header and Origin header.
const enum HostType {
  XForwardedHost = 'x-forwarded-host',
  Host = 'host',
}
type Host =
  | {
      type: HostType.XForwardedHost
      value: string
    }
  | {
      type: HostType.Host
      value: string
    }
  | undefined

/**
 * Ensures the value of the header can't create long logs.
 */
function limitUntrustedHeaderValueForLogs(value: string) {
  return value.length > 100 ? value.slice(0, 100) + '...' : value
}

export function parseHostHeader(
  headers: IncomingHttpHeaders,
  originDomain?: string
) {
  const forwardedHostHeader = headers['x-forwarded-host']
  const forwardedHostHeaderValue =
    forwardedHostHeader && Array.isArray(forwardedHostHeader)
      ? forwardedHostHeader[0]
      : forwardedHostHeader?.split(',')?.[0]?.trim()
  const hostHeader = headers['host']

  if (originDomain) {
    return forwardedHostHeaderValue === originDomain
      ? {
          type: HostType.XForwardedHost,
          value: forwardedHostHeaderValue,
        }
      : hostHeader === originDomain
        ? {
            type: HostType.Host,
            value: hostHeader,
          }
        : undefined
  }

  return forwardedHostHeaderValue
    ? {
        type: HostType.XForwardedHost,
        value: forwardedHostHeaderValue,
      }
    : hostHeader
      ? {
          type: HostType.Host,
          value: hostHeader,
        }
      : undefined
}

type ServerModuleMap = Record<
  string,
  {
    id: string
    chunks: string[]
    name: string
  }
>

type ServerActionsConfig = {
  bodySizeLimit?: SizeLimit
  allowedOrigins?: string[]
}

export async function handleAction({
  req,
  res,
  ComponentMod,
  serverModuleMap,
  generateFlight,
  workStore,
  requestStore,
  serverActions,
  ctx,
  metadata,
}: {
  req: BaseNextRequest
  res: BaseNextResponse
  ComponentMod: AppPageModule
  serverModuleMap: ServerModuleMap
  generateFlight: GenerateFlight
  workStore: WorkStore
  requestStore: RequestStore
  serverActions?: ServerActionsConfig
  ctx: AppRenderContext
  metadata: AppPageRenderResultMetadata
}): Promise<
  | undefined
  | {
      type: 'not-found'
    }
  | {
      type: 'done'
      result: RenderResult | undefined
      formState?: any
    }
> {
  const contentType = req.headers['content-type']
  const { serverActionsManifest, page } = ctx.renderOpts

  const {
    actionId,
    isURLEncodedAction,
    isMultipartAction,
    isFetchAction,
    isPossibleServerAction,
  } = getServerActionRequestMetadata(req)

  // If it can't be a Server Action, skip handling.
  // Note that this can be a false positive -- any multipart/urlencoded POST can get us here,
  // But won't know if it's an MPA action or not until we call `decodeAction` below.
  if (!isPossibleServerAction) {
    return
  }

  if (workStore.isStaticGeneration) {
    throw new Error(
      "Invariant: server actions can't be handled during static rendering"
    )
  }

  let temporaryReferences: TemporaryReferenceSet | undefined

  // When running actions the default is no-store, you can still `cache: 'force-cache'`
  workStore.fetchCache = 'default-no-store'

  const originDomain =
    typeof req.headers['origin'] === 'string'
      ? new URL(req.headers['origin']).host
      : undefined
  const host = parseHostHeader(req.headers)

  let warning: string | undefined = undefined

  function warnBadServerActionRequest() {
    if (warning) {
      warn(warning)
    }
  }
  // This is to prevent CSRF attacks. If `x-forwarded-host` is set, we need to
  // ensure that the request is coming from the same host.
  if (!originDomain) {
    // This might be an old browser that doesn't send `host` header. We ignore
    // this case.
    warning = 'Missing `origin` header from a forwarded Server Actions request.'
  } else if (!host || originDomain !== host.value) {
    // If the customer sets a list of allowed origins, we'll allow the request.
    // These are considered safe but might be different from forwarded host set
    // by the infra (i.e. reverse proxies).
    if (isCsrfOriginAllowed(originDomain, serverActions?.allowedOrigins)) {
      // Ignore it
    } else {
      if (host) {
        // This seems to be an CSRF attack. We should not proceed the action.
        console.error(
          `\`${
            host.type
          }\` header with value \`${limitUntrustedHeaderValueForLogs(
            host.value
          )}\` does not match \`origin\` header with value \`${limitUntrustedHeaderValueForLogs(
            originDomain
          )}\` from a forwarded Server Actions request. Aborting the action.`
        )
      } else {
        // This is an attack. We should not proceed the action.
        console.error(
          `\`x-forwarded-host\` or \`host\` headers are not provided. One of these is needed to compare the \`origin\` header from a forwarded Server Actions request. Aborting the action.`
        )
      }

      const error = new Error('Invalid Server Actions request.')

      if (isFetchAction) {
        res.statusCode = 500
        metadata.statusCode = 500

        const promise = Promise.reject(error)
        try {
          // we need to await the promise to trigger the rejection early
          // so that it's already handled by the time we call
          // the RSC runtime. Otherwise, it will throw an unhandled
          // promise rejection error in the renderer.
          await promise
        } catch {
          // swallow error, it's gonna be handled on the client
        }

        return {
          type: 'done',
          result: await generateFlight(req, ctx, requestStore, {
            actionResult: promise,
            // We didn't execute an action, so no revalidations could have occurred. We can skip rendering the page.
            skipFlight: true,
            temporaryReferences,
          }),
        }
      }

      throw error
    }
  }

  // ensure we avoid caching server actions unexpectedly
  res.setHeader(
    'Cache-Control',
    'no-cache, no-store, max-age=0, must-revalidate'
  )

  let boundActionArguments: unknown[] = []

  const { actionAsyncStorage } = ComponentMod

  let actionResult: RenderResult | undefined
  let formState: any | undefined
  let actionModId: string | undefined
  const actionWasForwarded = Boolean(req.headers['x-action-forwarded'])

  if (actionId) {
    const forwardedWorker = selectWorkerForForwarding(
      actionId,
      page,
      serverActionsManifest
    )

    // If forwardedWorker is truthy, it means there isn't a worker for the action
    // in the current handler, so we forward the request to a worker that has the action.
    if (forwardedWorker) {
      return {
        type: 'done',
        result: await createForwardedActionResponse(
          req,
          res,
          host,
          forwardedWorker,
          ctx.renderOpts.basePath
        ),
      }
    }
  }

  try {
    return await actionAsyncStorage.run({ isAction: true }, async () => {
      if (
        // The type check here ensures that `req` is correctly typed, and the
        // environment variable check provides dead code elimination.
        process.env.NEXT_RUNTIME === 'edge' &&
        isWebNextRequest(req)
      ) {
        if (!req.body) {
          throw new Error('invariant: Missing request body.')
        }

        // TODO: add body limit

        // Use react-server-dom-webpack/server.edge
        const {
          createTemporaryReferenceSet,
          decodeReply,
          decodeAction,
          decodeFormState,
        } = ComponentMod

        temporaryReferences = createTemporaryReferenceSet()

        if (isMultipartAction) {
          // TODO-APP: Add streaming support
          const formData = await req.request.formData()
          if (isFetchAction) {
            boundActionArguments = await decodeReply(
              formData,
              serverModuleMap,
              { temporaryReferences }
            )
          } else {
            let action: (() => unknown) | null = null

            try {
              action = await decodeAction(formData, serverModuleMap)
            } catch (err) {
              const unknownActionId = getActionIdFromReactError(err)

              if (unknownActionId) {
                console.warn(createUnknownServerActionError(unknownActionId))
                return { type: 'not-found' }
              }

              throw err
            }

            if (typeof action === 'function') {
              // Only warn if it's a server action, otherwise skip for other post requests
              warnBadServerActionRequest()

              const actionReturnedState =
                await executeActionAndPrepareForRender(
                  action as () => Promise<unknown>,
                  [],
                  workStore,
                  requestStore
                )

              formState = await decodeFormState(
                actionReturnedState,
                formData,
                serverModuleMap
              )
            }

            return { type: 'done', result: undefined, formState }
          }
        } else {
          if (!actionId) {
            // We don't consider this a server action when there's no actionId.
            return undefined
          }

          try {
            actionModId = getActionModIdOrError(actionId, serverModuleMap)
          } catch (err) {
            console.warn(err)
            return { type: 'not-found' }
          }

          const chunks: Buffer[] = []
          const reader = req.body.getReader()
          while (true) {
            const { done, value } = await reader.read()
            if (done) {
              break
            }

            chunks.push(value)
          }

          const actionData = Buffer.concat(chunks).toString('utf-8')

          if (isURLEncodedAction) {
            const formData = formDataFromSearchQueryString(actionData)
            boundActionArguments = await decodeReply(
              formData,
              serverModuleMap,
              { temporaryReferences }
            )
          } else {
            boundActionArguments = await decodeReply(
              actionData,
              serverModuleMap,
              { temporaryReferences }
            )
          }
        }
      } else if (
        // The type check here ensures that `req` is correctly typed, and the
        // environment variable check provides dead code elimination.
        process.env.NEXT_RUNTIME !== 'edge' &&
        isNodeNextRequest(req)
      ) {
        // Use react-server-dom-webpack/server.node which supports streaming
        const {
          createTemporaryReferenceSet,
          decodeReply,
          decodeReplyFromBusboy,
          decodeAction,
          decodeFormState,
        } = require(
          `./react-server.node`
        ) as typeof import('./react-server.node')

        temporaryReferences = createTemporaryReferenceSet()

        const { Transform, pipeline } =
          require('node:stream') as typeof import('node:stream')

        const defaultBodySizeLimit = '1 MB'
        const bodySizeLimit =
          serverActions?.bodySizeLimit ?? defaultBodySizeLimit
        const bodySizeLimitBytes =
          bodySizeLimit !== defaultBodySizeLimit
            ? (
                require('next/dist/compiled/bytes') as typeof import('next/dist/compiled/bytes')
              ).parse(bodySizeLimit)
            : 1024 * 1024 // 1 MB

        let size = 0
        const sizeLimitTransform = new Transform({
          transform(chunk, encoding, callback) {
            size += Buffer.byteLength(chunk, encoding)
            if (size > bodySizeLimitBytes) {
              const { ApiError } =
                require('../api-utils') as typeof import('../api-utils')

              callback(
                new ApiError(
                  413,
                  `Body exceeded ${bodySizeLimit} limit.\n` +
                    `To configure the body size limit for Server Actions, see: https://nextjs.org/docs/app/api-reference/next-config-js/serverActions#bodysizelimit`
                )
              )
              return
            }

            callback(null, chunk)
          },
        })

        const sizeLimitedBody = pipeline(
          req.body,
          sizeLimitTransform,
          // Avoid unhandled errors from `pipeline()` by passing an empty completion callback.
          // We'll propagate the errors properly when consuming the stream.
          () => {}
        )

        if (isMultipartAction) {
          if (isFetchAction) {
            const busboy = (
              require('next/dist/compiled/busboy') as typeof import('next/dist/compiled/busboy')
            )({
              defParamCharset: 'utf8',
              headers: req.headers,
              limits: { fieldSize: bodySizeLimitBytes },
            })

            // We need to use `pipeline(one, two)` instead of `one.pipe(two)` to propagate size limit errors correctly.
            pipeline(
              sizeLimitedBody,
              busboy,
              // Avoid unhandled errors from `pipeline()` by passing an empty completion callback.
              // We'll propagate the errors properly when consuming the stream.
              () => {}
            )

            boundActionArguments = await decodeReplyFromBusboy(
              busboy,
              serverModuleMap,
              { temporaryReferences }
            )
          } else {
            // React doesn't yet publish a busboy version of decodeAction
            // so we polyfill the parsing of FormData.
            const fakeRequest = new Request('http://localhost', {
              method: 'POST',
              // @ts-expect-error
              headers: { 'Content-Type': contentType },
              body: new ReadableStream({
                start: (controller) => {
                  sizeLimitedBody.on('data', (chunk) => {
                    controller.enqueue(new Uint8Array(chunk))
                  })
                  sizeLimitedBody.on('end', () => {
                    controller.close()
                  })
                  sizeLimitedBody.on('error', (err) => {
                    controller.error(err)
                  })
                },
              }),
              duplex: 'half',
            })

            const formData = await fakeRequest.formData()
            let action: (() => unknown) | null = null

            try {
              action = await decodeAction(formData, serverModuleMap)
            } catch (err) {
              const unknownActionId = getActionIdFromReactError(err)

              if (unknownActionId) {
                console.warn(createUnknownServerActionError(unknownActionId))
                return { type: 'not-found' }
              }

              throw err
            }

            if (typeof action === 'function') {
              // Only warn if it's a server action, otherwise skip for other post requests
              warnBadServerActionRequest()

              const actionReturnedState =
                await executeActionAndPrepareForRender(
                  action as () => Promise<unknown>,
                  [],
                  workStore,
                  requestStore
                )

              formState = await decodeFormState(
                actionReturnedState,
                formData,
                serverModuleMap
              )
            }

            return { type: 'done', result: undefined, formState }
          }
        } else {
          if (!actionId) {
            // We don't consider this a server action when there's no actionId.
            return undefined
          }

          try {
            actionModId = getActionModIdOrError(actionId, serverModuleMap)
          } catch (err) {
            console.warn(err)
            return { type: 'not-found' }
          }

          const chunks: Buffer[] = []
          for await (const chunk of sizeLimitedBody) {
            chunks.push(Buffer.from(chunk))
          }

          const actionData = Buffer.concat(chunks).toString('utf-8')

          if (isURLEncodedAction) {
            const formData = formDataFromSearchQueryString(actionData)
            boundActionArguments = await decodeReply(
              formData,
              serverModuleMap,
              { temporaryReferences }
            )
          } else {
            boundActionArguments = await decodeReply(
              actionData,
              serverModuleMap,
              { temporaryReferences }
            )
          }
        }
      } else {
        throw new Error('Invariant: Unknown request type.')
      }

      // actions.js
      // app/page.js
      //   action worker1
      //     appRender1

      // app/foo/page.js
      //   action worker2
      //     appRender

      // / -> fire action -> POST / -> appRender1 -> modId for the action file
      // /foo -> fire action -> POST /foo -> appRender2 -> modId for the action file

      if (!actionId) {
        // We don't consider this a server action when there's no actionId.
        return undefined
      }

      try {
        actionModId ??= getActionModIdOrError(actionId, serverModuleMap)
      } catch (err) {
        console.warn(err)
        return { type: 'not-found' }
      }

      const actionMod = (await ComponentMod.__next_app__.require(
        actionModId
      )) as Record<string, (...args: unknown[]) => Promise<unknown>>
      const actionHandler =
        actionMod[
          // `actionId` must exist if we got here, as otherwise we would have thrown an error above
          actionId!
        ]

      const returnVal = await executeActionAndPrepareForRender(
        actionHandler,
        boundActionArguments,
        workStore,
        requestStore
      ).finally(() => {
        addRevalidationHeader(res, { workStore, requestStore })
      })

      // For form actions, we need to continue rendering the page.
      if (isFetchAction) {
        actionResult = await generateFlight(req, ctx, requestStore, {
          actionResult: Promise.resolve(returnVal),
          // if the page was not revalidated, or if the action was forwarded from another worker, we can skip the rendering the flight tree
          skipFlight: !workStore.pathWasRevalidated || actionWasForwarded,
          temporaryReferences,
        })
      }

      return {
        type: 'done',
        result: actionResult,
        formState,
      }
    })
  } catch (err) {
    if (isRedirectError(err)) {
      const redirectUrl = getURLFromRedirectError(err)
      const redirectType = getRedirectTypeFromError(err)

      // if it's a fetch action, we'll set the status code for logging/debugging purposes
      // but we won't set a Location header, as the redirect will be handled by the client router
      res.statusCode = RedirectStatusCode.SeeOther
      metadata.statusCode = RedirectStatusCode.SeeOther

      if (isFetchAction) {
        return {
          type: 'done',
          result: await createRedirectRenderResult(
            req,
            res,
            host,
            redirectUrl,
            redirectType,
            ctx.renderOpts.basePath,
            workStore
          ),
        }
      }

      res.setHeader('Location', redirectUrl)
      return {
        type: 'done',
        result: RenderResult.fromStatic(''),
      }
    } else if (isHTTPAccessFallbackError(err)) {
      res.statusCode = getAccessFallbackHTTPStatus(err)
      metadata.statusCode = res.statusCode

      if (isFetchAction) {
        const promise = Promise.reject(err)
        try {
          // we need to await the promise to trigger the rejection early
          // so that it's already handled by the time we call
          // the RSC runtime. Otherwise, it will throw an unhandled
          // promise rejection error in the renderer.
          await promise
        } catch {
          // swallow error, it's gonna be handled on the client
        }
        return {
          type: 'done',
          result: await generateFlight(req, ctx, requestStore, {
            skipFlight: false,
            actionResult: promise,
            temporaryReferences,
          }),
        }
      }
      return {
        type: 'not-found',
      }
    }

    if (isFetchAction) {
      // TODO: consider checking if the error is an `ApiError` and change status code
      // so that we can respond with a 413 to requests that break the body size limit
      // (but if we do that, we also need to make sure that whatever handles the non-fetch error path below does the same)
      res.statusCode = 500
      metadata.statusCode = 500
      const promise = Promise.reject(err)
      try {
        // we need to await the promise to trigger the rejection early
        // so that it's already handled by the time we call
        // the RSC runtime. Otherwise, it will throw an unhandled
        // promise rejection error in the renderer.
        await promise
      } catch {
        // swallow error, it's gonna be handled on the client
      }

      return {
        type: 'done',
        result: await generateFlight(req, ctx, requestStore, {
          actionResult: promise,
          // if the page was not revalidated, or if the action was forwarded from another worker, we can skip the rendering the flight tree
          skipFlight: !workStore.pathWasRevalidated || actionWasForwarded,
          temporaryReferences,
        }),
      }
    }

    throw err
  }
}

async function executeActionAndPrepareForRender<
  TFn extends (...args: any[]) => Promise<any>,
>(
  action: TFn,
  args: Parameters<TFn>,
  workStore: WorkStore,
  requestStore: RequestStore
): Promise<Awaited<ReturnType<TFn>>> {
  requestStore.phase = 'action'
  try {
    return await workUnitAsyncStorage.run(requestStore, () =>
      action.apply(null, args)
    )
  } finally {
    requestStore.phase = 'render'

    // When we switch to the render phase, cookies() will return
    // `workUnitStore.cookies` instead of `workUnitStore.userspaceMutableCookies`.
    // We want the render to see any cookie writes that we performed during the action,
    // so we need to update the immutable cookies to reflect the changes.
    synchronizeMutableCookies(requestStore)

    // The server action might have toggled draft mode, so we need to reflect
    // that in the work store to be up-to-date for subsequent rendering.
    workStore.isDraftMode = requestStore.draftMode.isEnabled

    // If the action called revalidateTag/revalidatePath, then that might affect data used by the subsequent render,
    // so we need to make sure all revalidations are applied before that
    await executeRevalidates(workStore)
  }
}

function createUnknownServerActionError(actionId: string): Error {
  return new Error(
    `Failed to find Server Action "${actionId}". This request might be from an older or newer deployment.\nRead more: https://nextjs.org/docs/messages/failed-to-find-server-action`
  )
}

const unknownServerActionErrorMessageRegExp =
  /Could not find the module "(?<actionId>[0-9A-Fa-f]+)" in the React Server Manifest./

/**
 * Extract the action ID from a React error message that indicates the action ID
 * could not be found in the server module map. Brand-checking the error message
 * is a bit unfortunate, but there's currently no other way to detect this case
 * for MPA server actions, unless we duplicate React's form data parsing.
 */
function getActionIdFromReactError(err: unknown): string | undefined {
  if (err instanceof Error) {
    const match = err.message.match(unknownServerActionErrorMessageRegExp)

    if (match?.groups?.actionId) {
      return match.groups.actionId
    }
  }

  return undefined
}

/**
 * Attempts to find the module ID for the action from the module map. When this fails, it could be a deployment skew where
 * the action came from a different deployment. It could also simply be an invalid POST request that is not a server action.
 * In either case, we'll throw an error to be handled by the caller.
 */
function getActionModIdOrError(
  actionId: string,
  serverModuleMap: ServerModuleMap
): string {
  const actionModId = serverModuleMap[actionId]?.id

  if (!actionModId) {
    throw createUnknownServerActionError(actionId)
  }

  return actionModId
}
