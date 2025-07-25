import type { DeepReadonly } from '../../shared/lib/deep-readonly'
/* eslint-disable import/no-extraneous-dependencies */
import {
  renderToReadableStream,
  decodeReply,
  decodeReplyFromAsyncIterable,
  createTemporaryReferenceSet as createServerTemporaryReferenceSet,
} from 'react-server-dom-webpack/server.edge'
import {
  createFromReadableStream,
  encodeReply,
  createTemporaryReferenceSet as createClientTemporaryReferenceSet,
} from 'react-server-dom-webpack/client.edge'
import { unstable_prerender as prerender } from 'react-server-dom-webpack/static.edge'
/* eslint-enable import/no-extraneous-dependencies */

import type { WorkStore } from '../app-render/work-async-storage.external'
import { workAsyncStorage } from '../app-render/work-async-storage.external'
import type {
  UseCacheStore,
  WorkUnitStore,
} from '../app-render/work-unit-async-storage.external'
import {
  getHmrRefreshHash,
  getRenderResumeDataCache,
  getPrerenderResumeDataCache,
  workUnitAsyncStorage,
  getDraftModeProviderForCacheScope,
} from '../app-render/work-unit-async-storage.external'

import { makeHangingPromise } from '../dynamic-rendering-utils'

import type { ClientReferenceManifestForRsc } from '../../build/webpack/plugins/flight-manifest-plugin'

import {
  getClientReferenceManifestForRsc,
  getServerModuleMap,
} from '../app-render/encryption-utils'
import type { CacheEntry } from '../lib/cache-handlers/types'
import type { CacheSignal } from '../app-render/cache-signal'
import { decryptActionBoundArgs } from '../app-render/encryption'
import { InvariantError } from '../../shared/lib/invariant-error'
import { getDigestForWellKnownError } from '../app-render/create-error-handler'
import { DYNAMIC_EXPIRE } from './constants'
import { getCacheHandler } from './handlers'
import { UseCacheTimeoutError } from './use-cache-errors'
import { createHangingInputAbortSignal } from '../app-render/dynamic-rendering'
import {
  makeErroringExoticSearchParamsForUseCache,
  type SearchParams,
} from '../request/search-params'
import type { Params } from '../request/params'
import React from 'react'
import { createLazyResult, isResolvedLazyResult } from '../lib/lazy-result'
import { dynamicAccessAsyncStorage } from '../app-render/dynamic-access-async-storage.external'

type CacheKeyParts =
  | [buildId: string, id: string, args: unknown[]]
  | [buildId: string, id: string, args: unknown[], hmrRefreshHash: string]

export interface UseCachePageComponentProps {
  params: Promise<Params>
  searchParams: Promise<SearchParams>
  $$isPageComponent: true
}

export type UseCacheLayoutComponentProps = {
  params: Promise<Params>
  $$isLayoutComponent: true
} & {
  // The value type should be React.ReactNode. But such an index signature would
  // be incompatible with the other two props.
  [slot: string]: any
}

const isEdgeRuntime = process.env.NEXT_RUNTIME === 'edge'

const debug = process.env.NEXT_PRIVATE_DEBUG_CACHE
  ? console.debug.bind(console, 'use-cache:')
  : undefined

function generateCacheEntry(
  workStore: WorkStore,
  outerWorkUnitStore: WorkUnitStore | undefined,
  clientReferenceManifest: DeepReadonly<ClientReferenceManifestForRsc>,
  encodedArguments: FormData | string,
  fn: (...args: unknown[]) => Promise<unknown>,
  timeoutError: UseCacheTimeoutError
) {
  // We need to run this inside a clean AsyncLocalStorage snapshot so that the cache
  // generation cannot read anything from the context we're currently executing which
  // might include request specific things like cookies() inside a React.cache().
  // Note: It is important that we await at least once before this because it lets us
  // pop out of any stack specific contexts as well - aka "Sync" Local Storage.
  return workStore.runInCleanSnapshot(
    generateCacheEntryWithRestoredWorkStore,
    workStore,
    outerWorkUnitStore,
    clientReferenceManifest,
    encodedArguments,
    fn,
    timeoutError
  )
}

function generateCacheEntryWithRestoredWorkStore(
  workStore: WorkStore,
  outerWorkUnitStore: WorkUnitStore | undefined,
  clientReferenceManifest: DeepReadonly<ClientReferenceManifestForRsc>,
  encodedArguments: FormData | string,
  fn: (...args: unknown[]) => Promise<unknown>,
  timeoutError: UseCacheTimeoutError
) {
  // Since we cleared the AsyncLocalStorage we need to restore the workStore.
  // Note: We explicitly don't restore the RequestStore nor the PrerenderStore.
  // We don't want any request specific information leaking an we don't want to create a
  // bloated fake request mock for every cache call. So any feature that currently lives
  // in RequestStore but should be available to Caches need to move to WorkStore.
  // PrerenderStore is not needed inside the cache scope because the outer most one will
  // be the one to report its result to the outer Prerender.
  return workAsyncStorage.run(
    workStore,
    generateCacheEntryWithCacheContext,
    workStore,
    outerWorkUnitStore,
    clientReferenceManifest,
    encodedArguments,
    fn,
    timeoutError
  )
}

function generateCacheEntryWithCacheContext(
  workStore: WorkStore,
  outerWorkUnitStore: WorkUnitStore | undefined,
  clientReferenceManifest: DeepReadonly<ClientReferenceManifestForRsc>,
  encodedArguments: FormData | string,
  fn: (...args: unknown[]) => Promise<unknown>,
  timeoutError: UseCacheTimeoutError
) {
  if (!workStore.cacheLifeProfiles) {
    throw new Error(
      'cacheLifeProfiles should always be provided. This is a bug in Next.js.'
    )
  }
  const defaultCacheLife = workStore.cacheLifeProfiles['default']
  if (
    !defaultCacheLife ||
    defaultCacheLife.revalidate == null ||
    defaultCacheLife.expire == null ||
    defaultCacheLife.stale == null
  ) {
    throw new Error(
      'A default cacheLife profile must always be provided. This is a bug in Next.js.'
    )
  }

  const useCacheOrRequestStore =
    outerWorkUnitStore?.type === 'request' ||
    outerWorkUnitStore?.type === 'cache'
      ? outerWorkUnitStore
      : undefined

  // Initialize the Store for this Cache entry.
  const cacheStore: UseCacheStore = {
    type: 'cache',
    phase: 'render',
    implicitTags: outerWorkUnitStore?.implicitTags,
    revalidate: defaultCacheLife.revalidate,
    expire: defaultCacheLife.expire,
    stale: defaultCacheLife.stale,
    explicitRevalidate: undefined,
    explicitExpire: undefined,
    explicitStale: undefined,
    tags: null,
    hmrRefreshHash:
      outerWorkUnitStore && getHmrRefreshHash(workStore, outerWorkUnitStore),
    isHmrRefresh: useCacheOrRequestStore?.isHmrRefresh ?? false,
    serverComponentsHmrCache: useCacheOrRequestStore?.serverComponentsHmrCache,
    forceRevalidate: shouldForceRevalidate(workStore, outerWorkUnitStore),
    draftMode:
      outerWorkUnitStore &&
      getDraftModeProviderForCacheScope(workStore, outerWorkUnitStore),
  }

  return workUnitAsyncStorage.run(cacheStore, () =>
    dynamicAccessAsyncStorage.run(
      { abortController: new AbortController() },
      generateCacheEntryImpl,
      workStore,
      outerWorkUnitStore,
      cacheStore,
      clientReferenceManifest,
      encodedArguments,
      fn,
      timeoutError
    )
  )
}

function propagateCacheLifeAndTags(
  workUnitStore: WorkUnitStore | undefined,
  entry: CacheEntry
): void {
  if (
    workUnitStore &&
    (workUnitStore.type === 'cache' ||
      workUnitStore.type === 'prerender' ||
      workUnitStore.type === 'prerender-ppr' ||
      workUnitStore.type === 'prerender-legacy')
  ) {
    // Propagate tags and revalidate upwards
    const outerTags = workUnitStore.tags ?? (workUnitStore.tags = [])
    const entryTags = entry.tags
    for (let i = 0; i < entryTags.length; i++) {
      const tag = entryTags[i]
      if (!outerTags.includes(tag)) {
        outerTags.push(tag)
      }
    }
    if (workUnitStore.stale > entry.stale) {
      workUnitStore.stale = entry.stale
    }
    if (workUnitStore.revalidate > entry.revalidate) {
      workUnitStore.revalidate = entry.revalidate
    }
    if (workUnitStore.expire > entry.expire) {
      workUnitStore.expire = entry.expire
    }
  }
}

async function collectResult(
  savedStream: ReadableStream,
  workStore: WorkStore,
  outerWorkUnitStore: WorkUnitStore | undefined,
  innerCacheStore: UseCacheStore,
  startTime: number,
  errors: Array<unknown> // This is a live array that gets pushed into.
): Promise<CacheEntry> {
  // We create a buffered stream that collects all chunks until the end to
  // ensure that RSC has finished rendering and therefore we have collected
  // all tags. In the future the RSC API might allow for the equivalent of
  // the allReady Promise that exists on SSR streams.
  //
  // If something errored or rejected anywhere in the render, we close
  // the stream as errored. This lets a CacheHandler choose to save the
  // partial result up until that point for future hits for a while to avoid
  // unnecessary retries or not to retry. We use the end of the stream for
  // this to avoid another complicated side-channel. A receiver has to consider
  // that the stream might also error for other reasons anyway such as losing
  // connection.

  const buffer: any[] = []
  const reader = savedStream.getReader()

  try {
    for (let entry; !(entry = await reader.read()).done; ) {
      buffer.push(entry.value)
    }
  } catch (error) {
    errors.push(error)
  }

  let idx = 0
  const bufferStream = new ReadableStream({
    pull(controller) {
      if (workStore.invalidDynamicUsageError) {
        controller.error(workStore.invalidDynamicUsageError)
      } else if (idx < buffer.length) {
        controller.enqueue(buffer[idx++])
      } else if (errors.length > 0) {
        // TODO: Should we use AggregateError here?
        controller.error(errors[0])
      } else {
        controller.close()
      }
    },
  })

  const collectedTags = innerCacheStore.tags
  // If cacheLife() was used to set an explicit revalidate time we use that.
  // Otherwise, we use the lowest of all inner fetch()/unstable_cache() or nested "use cache".
  // If they're lower than our default.
  const collectedRevalidate =
    innerCacheStore.explicitRevalidate !== undefined
      ? innerCacheStore.explicitRevalidate
      : innerCacheStore.revalidate
  const collectedExpire =
    innerCacheStore.explicitExpire !== undefined
      ? innerCacheStore.explicitExpire
      : innerCacheStore.expire
  const collectedStale =
    innerCacheStore.explicitStale !== undefined
      ? innerCacheStore.explicitStale
      : innerCacheStore.stale

  const entry: CacheEntry = {
    value: bufferStream,
    timestamp: startTime,
    revalidate: collectedRevalidate,
    expire: collectedExpire,
    stale: collectedStale,
    tags: collectedTags === null ? [] : collectedTags,
  }

  // Propagate tags/revalidate to the parent context.
  propagateCacheLifeAndTags(outerWorkUnitStore, entry)

  const cacheSignal =
    outerWorkUnitStore && outerWorkUnitStore.type === 'prerender'
      ? outerWorkUnitStore.cacheSignal
      : null

  if (cacheSignal) {
    cacheSignal.endRead()
  }

  return entry
}

type GenerateCacheEntryResult =
  | {
      readonly type: 'cached'
      readonly stream: ReadableStream
      readonly pendingCacheEntry: Promise<CacheEntry>
    }
  | {
      readonly type: 'prerender-dynamic'
      readonly hangingPromise: Promise<never>
    }

async function generateCacheEntryImpl(
  workStore: WorkStore,
  outerWorkUnitStore: WorkUnitStore | undefined,
  innerCacheStore: UseCacheStore,
  clientReferenceManifest: DeepReadonly<ClientReferenceManifestForRsc>,
  encodedArguments: FormData | string,
  fn: (...args: unknown[]) => Promise<unknown>,
  timeoutError: UseCacheTimeoutError
): Promise<GenerateCacheEntryResult> {
  const temporaryReferences = createServerTemporaryReferenceSet()

  const [, , args] =
    typeof encodedArguments === 'string'
      ? await decodeReply<CacheKeyParts>(
          encodedArguments,
          getServerModuleMap(),
          { temporaryReferences }
        )
      : await decodeReplyFromAsyncIterable<CacheKeyParts>(
          {
            async *[Symbol.asyncIterator]() {
              for (const entry of encodedArguments) {
                yield entry
              }

              // The encoded arguments might contain hanging promises. In this
              // case we don't want to reject with "Error: Connection closed.",
              // so we intentionally keep the iterable alive. This is similar to
              // the halting trick that we do while rendering.
              if (outerWorkUnitStore?.type === 'prerender') {
                await new Promise<void>((resolve) => {
                  if (outerWorkUnitStore.renderSignal.aborted) {
                    resolve()
                  } else {
                    outerWorkUnitStore.renderSignal.addEventListener(
                      'abort',
                      () => resolve(),
                      { once: true }
                    )
                  }
                })
              }
            },
          },
          getServerModuleMap(),
          { temporaryReferences }
        )

  // Track the timestamp when we started computing the result.
  const startTime = performance.timeOrigin + performance.now()

  // Invoke the inner function to load a new result. We delay the invocation
  // though, until React awaits the promise so that React's request store (ALS)
  // is available when the function is invoked. This allows us, for example, to
  // capture logs so that we can later replay them.
  const resultPromise = createLazyResult(() => fn.apply(null, args))

  let errors: Array<unknown> = []

  // In the "Cache" environment, we only need to make sure that the error
  // digests are handled correctly. Error formatting and reporting is not
  // necessary here; the errors are encoded in the stream, and will be reported
  // in the "Server" environment.
  const handleError = (error: unknown): string | undefined => {
    const digest = getDigestForWellKnownError(error)

    if (digest) {
      return digest
    }

    if (process.env.NODE_ENV !== 'development') {
      // TODO: For now we're also reporting the error here, because in
      // production, the "Server" environment will only get the obfuscated
      // error (created by the Flight Client in the cache wrapper).
      console.error(error)
    }

    errors.push(error)
  }

  let stream: ReadableStream<Uint8Array>

  if (outerWorkUnitStore?.type === 'prerender') {
    const timeoutAbortController = new AbortController()

    // If we're prerendering, we give you 50 seconds to fill a cache entry.
    // Otherwise we assume you stalled on hanging input and de-opt. This needs
    // to be lower than just the general timeout of 60 seconds.
    const timer = setTimeout(() => {
      workStore.invalidDynamicUsageError = timeoutError
      timeoutAbortController.abort(timeoutError)
    }, 50000)

    const dynamicAccessAbortSignal =
      dynamicAccessAsyncStorage.getStore()?.abortController.signal

    const abortSignal = dynamicAccessAbortSignal
      ? AbortSignal.any([
          dynamicAccessAbortSignal,
          outerWorkUnitStore.renderSignal,
          timeoutAbortController.signal,
        ])
      : timeoutAbortController.signal

    const { prelude } = await prerender(
      resultPromise,
      clientReferenceManifest.clientModules,
      {
        environmentName: 'Cache',
        signal: abortSignal,
        temporaryReferences,
        onError(error) {
          if (abortSignal.aborted && abortSignal.reason === error) {
            return undefined
          }

          return handleError(error)
        },
      }
    )

    clearTimeout(timer)

    if (timeoutAbortController.signal.aborted) {
      // When the timeout is reached we always error the stream. Even for
      // fallback shell prerenders we don't want to return a hanging promise,
      // which would allow the function to become a dynamic hole. Because that
      // would mean that a non-empty shell could be generated which would be
      // subject to revalidation, and we don't want to create long revalidation
      // times.
      stream = new ReadableStream({
        start(controller) {
          controller.error(timeoutError)
        },
      })
    } else if (dynamicAccessAbortSignal?.aborted) {
      // If the prerender is aborted because of dynamic access (e.g. reading
      // fallback params), we return a hanging promise. This essentially makes
      // the "use cache" function dynamic.
      const hangingPromise = makeHangingPromise<never>(
        outerWorkUnitStore.renderSignal,
        abortSignal.reason
      )

      if (outerWorkUnitStore?.type === 'prerender') {
        outerWorkUnitStore.cacheSignal?.endRead()
      }

      return { type: 'prerender-dynamic', hangingPromise }
    } else {
      stream = prelude
    }
  } else {
    stream = renderToReadableStream(
      resultPromise,
      clientReferenceManifest.clientModules,
      {
        environmentName: 'Cache',
        temporaryReferences,
        onError: handleError,
      }
    )
  }

  const [returnStream, savedStream] = stream.tee()

  const pendingCacheEntry = collectResult(
    savedStream,
    workStore,
    outerWorkUnitStore,
    innerCacheStore,
    startTime,
    errors
  )

  return {
    type: 'cached',
    // Return the stream as we're creating it. This means that if it ends up
    // erroring we cannot return a stale-if-error version but it allows
    // streaming back the result earlier.
    stream: returnStream,
    pendingCacheEntry,
  }
}

function cloneCacheEntry(entry: CacheEntry): [CacheEntry, CacheEntry] {
  const [streamA, streamB] = entry.value.tee()
  entry.value = streamA
  const clonedEntry: CacheEntry = {
    value: streamB,
    timestamp: entry.timestamp,
    revalidate: entry.revalidate,
    expire: entry.expire,
    stale: entry.stale,
    tags: entry.tags,
  }
  return [entry, clonedEntry]
}

async function clonePendingCacheEntry(
  pendingCacheEntry: Promise<CacheEntry>
): Promise<[CacheEntry, CacheEntry]> {
  const entry = await pendingCacheEntry
  return cloneCacheEntry(entry)
}

async function getNthCacheEntry(
  split: Promise<[CacheEntry, CacheEntry]>,
  i: number
): Promise<CacheEntry> {
  return (await split)[i]
}

async function encodeFormData(formData: FormData): Promise<string> {
  let result = ''
  for (let [key, value] of formData) {
    // We don't need this key to be serializable but from a security perspective it should not be
    // possible to generate a string that looks the same from a different structure. To ensure this
    // we need a delimeter between fields but just using a delimeter is not enough since a string
    // might contain that delimeter. We use the length of each field as the delimeter to avoid
    // escaping the values.
    result += key.length.toString(16) + ':' + key
    let stringValue
    if (typeof value === 'string') {
      stringValue = value
    } else {
      // The FormData might contain binary data that is not valid UTF-8 so this cache
      // key may generate a UCS-2 string. Passing this to another service needs to be
      // aware that the key might not be compatible.
      const arrayBuffer = await value.arrayBuffer()
      if (arrayBuffer.byteLength % 2 === 0) {
        stringValue = String.fromCodePoint(...new Uint16Array(arrayBuffer))
      } else {
        stringValue =
          String.fromCodePoint(
            ...new Uint16Array(arrayBuffer, 0, (arrayBuffer.byteLength - 1) / 2)
          ) +
          String.fromCodePoint(
            new Uint8Array(arrayBuffer, arrayBuffer.byteLength - 1, 1)[0]
          )
      }
    }
    result += stringValue.length.toString(16) + ':' + stringValue
  }
  return result
}

function createTrackedReadableStream(
  stream: ReadableStream,
  cacheSignal: CacheSignal
) {
  const reader = stream.getReader()
  return new ReadableStream({
    async pull(controller) {
      const { done, value } = await reader.read()
      if (done) {
        controller.close()
        cacheSignal.endRead()
      } else {
        controller.enqueue(value)
      }
    },
  })
}

export function cache(
  kind: string,
  id: string,
  boundArgsLength: number,
  originalFn: (...args: unknown[]) => Promise<unknown>
) {
  const cacheHandler = getCacheHandler(kind)
  if (cacheHandler === undefined) {
    throw new Error('Unknown cache handler: ' + kind)
  }

  // Capture the timeout error here to ensure a useful stack.
  const timeoutError = new UseCacheTimeoutError()
  Error.captureStackTrace(timeoutError, cache)

  const name = originalFn.name
  const cachedFn = {
    [name]: async function (...args: any[]) {
      const workStore = workAsyncStorage.getStore()
      if (workStore === undefined) {
        throw new Error(
          '"use cache" cannot be used outside of App Router. Expected a WorkStore.'
        )
      }

      let fn = originalFn

      const workUnitStore = workUnitAsyncStorage.getStore()

      // Get the clientReferenceManifest while we're still in the outer Context.
      // In case getClientReferenceManifestSingleton is implemented using AsyncLocalStorage.
      const clientReferenceManifest = getClientReferenceManifestForRsc()

      // Because the Action ID is not yet unique per implementation of that Action we can't
      // safely reuse the results across builds yet. In the meantime we add the buildId to the
      // arguments as a seed to ensure they're not reused. Remove this once Action IDs hash
      // the implementation.
      const buildId = workStore.buildId

      // In dev mode, when the HMR refresh hash is set, we include it in the
      // cache key. This ensures that cache entries are not reused when server
      // components have been edited. This is a very coarse approach. But it's
      // also only a temporary solution until Action IDs are unique per
      // implementation. Remove this once Action IDs hash the implementation.
      const hmrRefreshHash =
        workUnitStore && getHmrRefreshHash(workStore, workUnitStore)

      const hangingInputAbortSignal =
        workUnitStore?.type === 'prerender'
          ? createHangingInputAbortSignal(workUnitStore)
          : undefined

      let isPageOrLayout = false

      // For page and layout components, the cache function is overwritten,
      // which allows us to apply special handling for params and searchParams.
      // For pages and layouts we're using the outer params prop, and not the
      // inner one that was serialized/deserialized. While it's not generally
      // true for "use cache" args, in the case of `params` the inner and outer
      // object are essentially equivalent, so this is safe to do (including
      // fallback params that are hanging promises). It allows us to avoid
      // waiting for the timeout, when prerendering a fallback shell of a cached
      // page or layout that awaits params.
      if (isPageComponent(args)) {
        isPageOrLayout = true

        const [{ params: outerParams, searchParams: outerSearchParams }] = args
        // Overwrite the props to omit $$isPageComponent.
        args = [{ params: outerParams, searchParams: outerSearchParams }]

        fn = {
          [name]: async ({
            params: _innerParams,
            searchParams: innerSearchParams,
          }: Omit<UseCachePageComponentProps, '$$isPageComponent'>) =>
            originalFn.apply(null, [
              {
                params: outerParams,
                searchParams: workStore.dynamicIOEnabled
                  ? innerSearchParams
                  : // When dynamicIO is not enabled, we can not encode
                    // searchParams as a hanging promise. To still avoid unused
                    // search params from making a page dynamic, we define them
                    // in `createComponentTree` as a promise that resolves to an
                    // empty object. And here, we're creating an erroring
                    // searchParams prop, when invoking the original function.
                    // This ensures that used searchParams inside of cached
                    // functions would still yield an error.
                    makeErroringExoticSearchParamsForUseCache(workStore),
              },
            ]),
        }[name] as (...args: unknown[]) => Promise<unknown>
      } else if (isLayoutComponent(args)) {
        isPageOrLayout = true

        const [{ params: outerParams, $$isLayoutComponent, ...outerSlots }] =
          args
        // Overwrite the props to omit $$isLayoutComponent.
        args = [{ params: outerParams, ...outerSlots }]

        fn = {
          [name]: async ({
            params: _innerParams,
            ...innerSlots
          }: Omit<UseCacheLayoutComponentProps, '$$isLayoutComponent'>) =>
            originalFn.apply(null, [{ params: outerParams, ...innerSlots }]),
        }[name] as (...args: unknown[]) => Promise<unknown>
      }

      if (boundArgsLength > 0) {
        if (args.length === 0) {
          throw new InvariantError(
            `Expected the "use cache" function ${JSON.stringify(fn.name)} to receive its encrypted bound arguments as the first argument.`
          )
        }

        const encryptedBoundArgs = args.shift()
        const boundArgs = await decryptActionBoundArgs(id, encryptedBoundArgs)

        if (!Array.isArray(boundArgs)) {
          throw new InvariantError(
            `Expected the bound arguments of "use cache" function ${JSON.stringify(fn.name)} to deserialize into an array, got ${typeof boundArgs} instead.`
          )
        }

        if (boundArgsLength !== boundArgs.length) {
          throw new InvariantError(
            `Expected the "use cache" function ${JSON.stringify(fn.name)} to receive ${boundArgsLength} bound arguments, got ${boundArgs.length} instead.`
          )
        }

        args.unshift(boundArgs)
      }

      const temporaryReferences = createClientTemporaryReferenceSet()

      const cacheKeyParts: CacheKeyParts = hmrRefreshHash
        ? [buildId, id, args, hmrRefreshHash]
        : [buildId, id, args]

      const encodeCacheKeyParts = () =>
        encodeReply(cacheKeyParts, {
          temporaryReferences,
          signal: hangingInputAbortSignal,
        })

      let encodedCacheKeyParts: FormData | string

      if (workUnitStore?.type === 'prerender' && !isPageOrLayout) {
        // If the "use cache" function is not a page or a layout, we need to
        // track dynamic access already when encoding the arguments. If params
        // are passed explicitly into a "use cache" function (as opposed to
        // receiving them automatically in a page or layout), we assume that the
        // params are also accessed. This allows us to abort early, and treat
        // the function as dynamic, instead of waiting for the timeout to be
        // reached.
        const dynamicAccessAbortController = new AbortController()

        encodedCacheKeyParts = await dynamicAccessAsyncStorage.run(
          { abortController: dynamicAccessAbortController },
          encodeCacheKeyParts
        )

        if (dynamicAccessAbortController.signal.aborted) {
          return makeHangingPromise(
            workUnitStore.renderSignal,
            dynamicAccessAbortController.signal.reason.message
          )
        }
      } else {
        encodedCacheKeyParts = await encodeCacheKeyParts()
      }

      const serializedCacheKey =
        typeof encodedCacheKeyParts === 'string'
          ? // Fast path for the simple case for simple inputs. We let the CacheHandler
            // Convert it to an ArrayBuffer if it wants to.
            encodedCacheKeyParts
          : await encodeFormData(encodedCacheKeyParts)

      let stream: undefined | ReadableStream = undefined

      // Get an immutable and mutable versions of the resume data cache.
      const prerenderResumeDataCache = workUnitStore
        ? getPrerenderResumeDataCache(workUnitStore)
        : null
      const renderResumeDataCache = workUnitStore
        ? getRenderResumeDataCache(workUnitStore)
        : null

      if (renderResumeDataCache) {
        const cacheSignal =
          workUnitStore && workUnitStore.type === 'prerender'
            ? workUnitStore.cacheSignal
            : null

        if (cacheSignal) {
          cacheSignal.beginRead()
        }
        const cachedEntry = renderResumeDataCache.cache.get(serializedCacheKey)
        if (cachedEntry !== undefined) {
          const existingEntry = await cachedEntry
          propagateCacheLifeAndTags(workUnitStore, existingEntry)
          if (
            workUnitStore !== undefined &&
            workUnitStore.type === 'prerender' &&
            existingEntry !== undefined &&
            (existingEntry.revalidate === 0 ||
              existingEntry.expire < DYNAMIC_EXPIRE)
          ) {
            // In a Dynamic I/O prerender, if the cache entry has revalidate: 0 or if the
            // expire time is under 5 minutes, then we consider this cache entry dynamic
            // as it's not worth generating static pages for such data. It's better to leave
            // a PPR hole that can be filled in dynamically with a potentially cached entry.
            if (cacheSignal) {
              cacheSignal.endRead()
            }
            return makeHangingPromise(
              workUnitStore.renderSignal,
              'dynamic "use cache"'
            )
          }
          const [streamA, streamB] = existingEntry.value.tee()
          existingEntry.value = streamB

          if (cacheSignal) {
            // When we have a cacheSignal we need to block on reading the cache
            // entry before ending the read.
            stream = createTrackedReadableStream(streamA, cacheSignal)
          } else {
            stream = streamA
          }
        } else {
          if (cacheSignal) {
            cacheSignal.endRead()
          }

          // If `allowEmptyStaticShell` is true, and a prefilled resume data
          // cache was provided, then a cache miss means that params were part
          // of the cache key. In this case, we can make this cache function a
          // dynamic hole in the shell (or produce an empty shell if there's no
          // parent suspense boundary). Currently, this also includes layouts
          // and pages that don't read params, which will be improved when we
          // implement NAR-136. Otherwise, we assume that if params are passed
          // explicitly into a "use cache" function, that the params are also
          // accessed. This allows us to abort early, and treat the function as
          // dynamic, instead of waiting for the timeout to be reached. Compared
          // to the instrumentation-based params bailout we do here, this also
          // covers the case where params are transformed with an async
          // function, before being passed into the "use cache" function, which
          // escapes the instrumentation.
          if (
            workUnitStore?.type === 'prerender' &&
            workUnitStore.allowEmptyStaticShell
          ) {
            return makeHangingPromise(
              workUnitStore.renderSignal,
              'dynamic "use cache"'
            )
          }
        }
      }

      if (stream === undefined) {
        const cacheSignal =
          workUnitStore && workUnitStore.type === 'prerender'
            ? workUnitStore.cacheSignal
            : null
        if (cacheSignal) {
          // Either the cache handler or the generation can be using I/O at this point.
          // We need to track when they start and when they complete.
          cacheSignal.beginRead()
        }

        const lazyRefreshTags = workStore.refreshTagsByCacheKind.get(kind)

        if (lazyRefreshTags && !isResolvedLazyResult(lazyRefreshTags)) {
          await lazyRefreshTags
        }

        let entry = shouldForceRevalidate(workStore, workUnitStore)
          ? undefined
          : await cacheHandler.get(
              serializedCacheKey,
              workUnitStore?.implicitTags?.tags ?? []
            )

        if (entry) {
          const implicitTags = workUnitStore?.implicitTags?.tags ?? []
          let implicitTagsExpiration = 0

          if (workUnitStore?.implicitTags) {
            const lazyExpiration =
              workUnitStore.implicitTags.expirationsByCacheKind.get(kind)

            if (lazyExpiration) {
              const expiration = isResolvedLazyResult(lazyExpiration)
                ? lazyExpiration.value
                : await lazyExpiration

              // If a cache handler returns an expiration time of Infinity, it
              // signals to Next.js that it handles checking cache entries for
              // staleness based on the expiration of the implicit tags passed
              // into the `get` method. In this case, we keep the default of 0,
              // which means that the implicit tags are not considered expired.
              if (expiration < Infinity) {
                implicitTagsExpiration = expiration
              }
            }
          }

          if (
            shouldDiscardCacheEntry(
              entry,
              workStore,
              implicitTags,
              implicitTagsExpiration
            )
          ) {
            debug?.('discarding stale entry', serializedCacheKey)
            entry = undefined
          }
        }

        const currentTime = performance.timeOrigin + performance.now()
        if (
          workUnitStore !== undefined &&
          workUnitStore.type === 'prerender' &&
          entry !== undefined &&
          (entry.revalidate === 0 || entry.expire < DYNAMIC_EXPIRE)
        ) {
          // In a Dynamic I/O prerender, if the cache entry has revalidate: 0 or if the
          // expire time is under 5 minutes, then we consider this cache entry dynamic
          // as it's not worth generating static pages for such data. It's better to leave
          // a PPR hole that can be filled in dynamically with a potentially cached entry.
          if (cacheSignal) {
            cacheSignal.endRead()
          }

          return makeHangingPromise(
            workUnitStore.renderSignal,
            'dynamic "use cache"'
          )
        } else if (
          entry === undefined ||
          currentTime > entry.timestamp + entry.expire * 1000 ||
          (workStore.isStaticGeneration &&
            currentTime > entry.timestamp + entry.revalidate * 1000)
        ) {
          // Miss. Generate a new result.

          // If the cache entry is stale and we're prerendering, we don't want to use the
          // stale entry since it would unnecessarily need to shorten the lifetime of the
          // prerender. We're not time constrained here so we can re-generated it now.

          // We need to run this inside a clean AsyncLocalStorage snapshot so that the cache
          // generation cannot read anything from the context we're currently executing which
          // might include request specific things like cookies() inside a React.cache().
          // Note: It is important that we await at least once before this because it lets us
          // pop out of any stack specific contexts as well - aka "Sync" Local Storage.

          if (entry) {
            if (currentTime > entry.timestamp + entry.expire * 1000) {
              debug?.('entry is expired', serializedCacheKey)
            }

            if (
              workStore.isStaticGeneration &&
              currentTime > entry.timestamp + entry.revalidate * 1000
            ) {
              debug?.('static generation, entry is stale', serializedCacheKey)
            }
          }

          const result = await generateCacheEntry(
            workStore,
            workUnitStore,
            clientReferenceManifest,
            encodedCacheKeyParts,
            fn,
            timeoutError
          )

          if (result.type === 'prerender-dynamic') {
            return result.hangingPromise
          }

          const { stream: newStream, pendingCacheEntry } = result

          // When draft mode is enabled, we must not save the cache entry.
          if (!workStore.isDraftMode) {
            let savedCacheEntry

            if (prerenderResumeDataCache) {
              // Create a clone that goes into the cache scope memory cache.
              const split = clonePendingCacheEntry(pendingCacheEntry)
              savedCacheEntry = getNthCacheEntry(split, 0)
              prerenderResumeDataCache.cache.set(
                serializedCacheKey,
                getNthCacheEntry(split, 1)
              )
            } else {
              savedCacheEntry = pendingCacheEntry
            }

            const promise = cacheHandler.set(
              serializedCacheKey,
              savedCacheEntry
            )

            workStore.pendingRevalidateWrites ??= []
            workStore.pendingRevalidateWrites.push(promise)
          }

          stream = newStream
        } else {
          propagateCacheLifeAndTags(workUnitStore, entry)

          // We want to return this stream, even if it's stale.
          stream = entry.value

          // If we have a cache scope, we need to clone the entry and set it on
          // the inner cache scope.
          if (prerenderResumeDataCache) {
            const [entryLeft, entryRight] = cloneCacheEntry(entry)
            if (cacheSignal) {
              stream = createTrackedReadableStream(entryLeft.value, cacheSignal)
            } else {
              stream = entryLeft.value
            }

            prerenderResumeDataCache.cache.set(
              serializedCacheKey,
              Promise.resolve(entryRight)
            )
          } else {
            // If we're not regenerating we need to signal that we've finished
            // putting the entry into the cache scope at this point. Otherwise we do
            // that inside generateCacheEntry.
            cacheSignal?.endRead()
          }

          if (currentTime > entry.timestamp + entry.revalidate * 1000) {
            // If this is stale, and we're not in a prerender (i.e. this is
            // dynamic render), then we should warm up the cache with a fresh
            // revalidated entry.
            const result = await generateCacheEntry(
              workStore,
              // This is not running within the context of this unit.
              undefined,
              clientReferenceManifest,
              encodedCacheKeyParts,
              fn,
              timeoutError
            )

            if (result.type === 'cached') {
              const { stream: ignoredStream, pendingCacheEntry } = result
              let savedCacheEntry: Promise<CacheEntry>

              if (prerenderResumeDataCache) {
                const split = clonePendingCacheEntry(pendingCacheEntry)
                savedCacheEntry = getNthCacheEntry(split, 0)
                prerenderResumeDataCache.cache.set(
                  serializedCacheKey,
                  getNthCacheEntry(split, 1)
                )
              } else {
                savedCacheEntry = pendingCacheEntry
              }

              const promise = cacheHandler.set(
                serializedCacheKey,
                savedCacheEntry
              )

              workStore.pendingRevalidateWrites ??= []
              workStore.pendingRevalidateWrites.push(promise)

              await ignoredStream.cancel()
            }
          }
        }
      }

      // Logs are replayed even if it's a hit - to ensure we see them on the client eventually.
      // If we didn't then the client wouldn't see the logs if it was seeded from a prewarm that
      // never made it to the client. However, this also means that you see logs even when the
      // cached function isn't actually re-executed. We should instead ensure prewarms always
      // make it to the client. Another issue is that this will cause double logging in the
      // server terminal. Once while generating the cache entry and once when replaying it on
      // the server, which is required to pick it up for replaying again on the client.
      const replayConsoleLogs = true

      const serverConsumerManifest = {
        // moduleLoading must be null because we don't want to trigger preloads of ClientReferences
        // to be added to the consumer. Instead, we'll wait for any ClientReference to be emitted
        // which themselves will handle the preloading.
        moduleLoading: null,
        moduleMap: isEdgeRuntime
          ? clientReferenceManifest.edgeRscModuleMapping
          : clientReferenceManifest.rscModuleMapping,
        serverModuleMap: getServerModuleMap(),
      }

      return createFromReadableStream(stream, {
        serverConsumerManifest,
        temporaryReferences,
        replayConsoleLogs,
        environmentName: 'Cache',
      })
    },
  }[name]

  return React.cache(cachedFn)
}

function isPageComponent(
  args: any[]
): args is [UseCachePageComponentProps, undefined] {
  if (args.length !== 2) {
    return false
  }

  const [props, ref] = args

  return (
    ref === undefined && // server components receive an undefined ref arg
    props !== null &&
    typeof props === 'object' &&
    (props as UseCachePageComponentProps).$$isPageComponent
  )
}

function isLayoutComponent(
  args: any[]
): args is [UseCacheLayoutComponentProps, undefined] {
  if (args.length !== 2) {
    return false
  }

  const [props, ref] = args

  return (
    ref === undefined && // server components receive an undefined ref arg
    props !== null &&
    typeof props === 'object' &&
    (props as UseCacheLayoutComponentProps).$$isLayoutComponent
  )
}

function shouldForceRevalidate(
  workStore: WorkStore,
  workUnitStore: WorkUnitStore | undefined
): boolean {
  if (workStore.isOnDemandRevalidate || workStore.isDraftMode) {
    return true
  }

  if (workStore.dev && workUnitStore) {
    if (workUnitStore.type === 'request') {
      return workUnitStore.headers.get('cache-control') === 'no-cache'
    }

    if (workUnitStore.type === 'cache') {
      return workUnitStore.forceRevalidate
    }
  }

  return false
}

function shouldDiscardCacheEntry(
  entry: CacheEntry,
  workStore: WorkStore,
  implicitTags: string[],
  implicitTagsExpiration: number
): boolean {
  // If the cache entry contains revalidated tags that the cache handler might
  // not know about yet, we need to discard it.
  if (entry.tags.some((tag) => isRecentlyRevalidatedTag(tag, workStore))) {
    return true
  }

  // If the cache entry was created before any of the implicit tags were
  // revalidated last, we also need to discard it.
  if (entry.timestamp <= implicitTagsExpiration) {
    debug?.(
      'entry was created at',
      entry.timestamp,
      'before implicit tags were revalidated at',
      implicitTagsExpiration
    )

    return true
  }

  // Finally, if any of the implicit tags have been revalidated recently, we
  // also need to discard the cache entry.
  if (implicitTags.some((tag) => isRecentlyRevalidatedTag(tag, workStore))) {
    return true
  }

  return false
}

function isRecentlyRevalidatedTag(tag: string, workStore: WorkStore): boolean {
  const { previouslyRevalidatedTags, pendingRevalidatedTags } = workStore

  // Was the tag previously revalidated (e.g. by a redirecting server action)?
  if (previouslyRevalidatedTags.includes(tag)) {
    debug?.('tag', tag, 'was previously revalidated')

    return true
  }

  // It could also have been revalidated by the currently running server action.
  // In this case the revalidation might not have been propagated to the cache
  // handler yet, so we read it from the pending tags in the work store.
  if (pendingRevalidatedTags?.includes(tag)) {
    debug?.('tag', tag, 'was just revalidated')

    return true
  }

  return false
}
