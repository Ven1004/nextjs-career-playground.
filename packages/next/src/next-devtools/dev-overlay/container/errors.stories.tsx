import type { Meta, StoryObj } from '@storybook/react'

import { Errors } from './errors'
import { withShadowPortal } from '../storybook/with-shadow-portal'
import type { ReadyRuntimeError } from '../utils/get-error-by-type'
import { lorem } from '../utils/lorem'

const meta: Meta<typeof Errors> = {
  component: Errors,
  parameters: {
    layout: 'fullscreen',
  },
  decorators: [withShadowPortal],
}

export default meta
type Story = StoryObj<typeof Errors>

const originalCodeFrame = (message: string) => {
  return `\u001b[0m \u001b[90m 1 \u001b[39m \u001b[36mexport\u001b[39m \u001b[36mdefault\u001b[39m \u001b[36mfunction\u001b[39m \u001b[33mHome\u001b[39m() {\u001b[0m
\u001b[0m\u001b[31m\u001b[1m>\u001b[22m\u001b[39m\u001b[90m 2 \u001b[39m   \u001b[36mthrow\u001b[39m \u001b[36mnew\u001b[39m \u001b[33mError\u001b[39m(\u001b[32m'${message}'\u001b[39m)\u001b[0m
\u001b[0m \u001b[90m   \u001b[39m         \u001b[31m\u001b[1m^\u001b[22m\u001b[39m\u001b[0m
\u001b[0m \u001b[90m 3 \u001b[39m   \u001b[36mreturn\u001b[39m \u001b[33m<\u001b[39m\u001b[33mdiv\u001b[39m\u001b[33m>\u001b[39m\u001b[33mWelcome to my Next.js application! This is a longer piece of text that will demonstrate text wrapping behavior in the code frame.\u001b[39m\u001b[33m<\u001b[39m\u001b[33m/\u001b[39m\u001b[33mdiv\u001b[39m\u001b[33m>\u001b[39m\u001b[0m
\u001b[0m \u001b[90m 4 \u001b[39m }\u001b[0m
\u001b[0m \u001b[90m 5 \u001b[39m\u001b[0m`
}

const sourceStackFrame = {
  file: 'app/page.tsx',
  methodName: 'Home',
  arguments: [],
  lineNumber: 2,
  column: 9,
}

const originalStackFrame = {
  file: 'app/page.tsx',
  methodName: 'Home',
  arguments: [],
  lineNumber: 2,
  column: 9,
  ignored: false,
}

const frame = {
  originalStackFrame: {
    file: './app/page.tsx',
    methodName: 'MyComponent',
    arguments: [],
    lineNumber: 10,
    column: 5,
    ignored: false,
  },
  sourceStackFrame: {
    file: './app/page.tsx',
    methodName: 'MyComponent',
    arguments: [],
    lineNumber: 10,
    column: 5,
  },
  originalCodeFrame: 'export default function MyComponent() {',
  error: false,
  reason: null,
  external: false,
  ignored: false,
}

const ignoredFrame = {
  ...frame,
  ignored: true,
}

const runtimeErrors: ReadyRuntimeError[] = [
  {
    id: 1,
    runtime: true,
    error: new Error('First error message'),
    frames: () =>
      Promise.resolve([
        frame,
        {
          ...frame,
          originalStackFrame: {
            ...frame.originalStackFrame,
            methodName: 'ParentComponent',
            lineNumber: 5,
          },
        },
        {
          ...frame,
          originalStackFrame: {
            ...frame.originalStackFrame,
            methodName: 'GrandparentComponent',
            lineNumber: 1,
          },
        },
        ...Array(20).fill(ignoredFrame),
      ]),
    type: 'runtime',
  },
  {
    id: 2,
    runtime: true,
    error: new Error('Second error message'),
    frames: () =>
      Promise.resolve([
        {
          error: true,
          reason: 'Second error message',
          external: false,
          ignored: false,
          sourceStackFrame,
          originalStackFrame,
          originalCodeFrame: originalCodeFrame('Second error message'),
        },
      ]),
    type: 'runtime',
  },
  {
    id: 3,
    runtime: true,
    error: new Error('Third error message'),
    frames: () =>
      Promise.resolve([
        {
          error: true,
          reason: 'Third error message',
          external: false,
          ignored: false,
          sourceStackFrame,
          originalStackFrame,
          originalCodeFrame: originalCodeFrame('Third error message'),
        },
      ]),
    type: 'runtime',
  },
  {
    id: 4,
    runtime: true,
    error: new Error('typeof window !== undefined'),
    frames: () =>
      Promise.resolve([
        {
          error: true,
          reason: 'typeof window !== undefined',
          external: false,
          ignored: false,
          sourceStackFrame,
          originalStackFrame,
          originalCodeFrame: originalCodeFrame('typeof window !== undefined'),
        },
      ]),
    type: 'runtime',
  },
]

export const Default: Story = {
  args: {
    getSquashedHydrationErrorDetails: () => null,
    runtimeErrors,
    versionInfo: {
      installed: '15.0.0',
      staleness: 'fresh',
    },
    debugInfo: { devtoolsFrontendUrl: undefined },
    isTurbopack: false,
    onClose: () => {},
  },
}

export const Turbopack: Story = {
  args: {
    ...Default.args,
    isTurbopack: true,
  },
}

export const VeryLongErrorMessage: Story = {
  args: {
    ...Default.args,
    runtimeErrors: [
      {
        ...runtimeErrors[0],
        error: Object.assign(new Error(lorem)),
      },
    ],
  },
}

export const WithHydrationWarning: Story = {
  args: {
    ...Default.args,
    runtimeErrors: [
      {
        id: 1,
        runtime: true,
        error: Object.assign(new Error('Hydration error'), {
          details: {
            warning: [
              'Text content does not match server-rendered HTML: "%s" !== "%s"',
              'Server Content',
              'Client Content',
            ],
            reactOutputComponentDiff: `<MyComponent>
  <ParentComponent>
    <div>
-     <p> hello world and welcome to my amazing website with lots of content hello world and welcome to my amazing website with lots of content </p>
+     <div> hello world and welcome to my amazing website with lots of content hello world and welcome to my amazing website with lots of content </div>`,
          },
          componentStackFrames: [
            {
              component: 'MyComponent',
              file: 'app/page.tsx',
              lineNumber: 10,
              columnNumber: 5,
            },
            {
              component: 'ParentComponent',
              file: 'app/layout.tsx',
              lineNumber: 20,
              columnNumber: 3,
            },
          ],
        }),
        frames: () =>
          Promise.resolve([
            {
              error: true,
              reason: 'First error message',
              external: false,
              ignored: false,
              sourceStackFrame: {
                file: 'app/page.tsx',
                methodName: 'Home',
                arguments: [],
                lineNumber: 10,
                column: 5,
              },
            },
          ]),
        type: 'runtime',
      },
    ],
    debugInfo: { devtoolsFrontendUrl: undefined },
    onClose: () => {},
  },
}
