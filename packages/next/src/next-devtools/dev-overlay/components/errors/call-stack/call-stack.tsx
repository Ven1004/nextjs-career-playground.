import type { OriginalStackFrame } from '../../../../shared/stack-frame'
import { useMemo, useState, useRef } from 'react'
import { CallStackFrame } from '../../call-stack-frame/call-stack-frame'

interface CallStackProps {
  frames: OriginalStackFrame[]
  dialogResizerRef: React.RefObject<HTMLDivElement | null>
}

export function CallStack({ frames, dialogResizerRef }: CallStackProps) {
  const initialDialogHeight = useRef<number>(NaN)
  const [isIgnoreListOpen, setIsIgnoreListOpen] = useState(false)

  const ignoredFramesTally = useMemo(() => {
    return frames.reduce((tally, frame) => tally + (frame.ignored ? 1 : 0), 0)
  }, [frames])

  function onToggleIgnoreList() {
    const dialog = dialogResizerRef?.current as HTMLElement

    if (!dialog) {
      return
    }

    const { height: currentHeight } = dialog?.getBoundingClientRect()

    if (!initialDialogHeight.current) {
      initialDialogHeight.current = currentHeight
    }

    if (isIgnoreListOpen) {
      function onTransitionEnd() {
        dialog.removeEventListener('transitionend', onTransitionEnd)
        setIsIgnoreListOpen(false)
      }
      dialog.style.height = `${initialDialogHeight.current}px`
      dialog.addEventListener('transitionend', onTransitionEnd)
    } else {
      setIsIgnoreListOpen(true)
    }
  }

  return (
    <div className="error-overlay-call-stack-container">
      <div className="error-overlay-call-stack-header">
        <p className="error-overlay-call-stack-title">
          Call Stack{' '}
          <span className="error-overlay-call-stack-count">
            {frames.length}
          </span>
        </p>
        {ignoredFramesTally > 0 && (
          <button
            data-expand-ignore-button={isIgnoreListOpen}
            className="error-overlay-call-stack-ignored-list-toggle-button"
            onClick={onToggleIgnoreList}
          >
            {`${isIgnoreListOpen ? 'Hide' : 'Show'} ${ignoredFramesTally} ignore-listed frame(s)`}
            <ChevronUpDown />
          </button>
        )}
      </div>
      {frames.map((frame, frameIndex) => {
        return !frame.ignored || isIgnoreListOpen ? (
          <CallStackFrame key={frameIndex} frame={frame} />
        ) : null
      })}
    </div>
  )
}

function ChevronUpDown() {
  return (
    <svg
      width="16"
      height="16"
      viewBox="0 0 16 16"
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
    >
      <path
        fillRule="evenodd"
        clipRule="evenodd"
        d="M8.70722 2.39641C8.3167 2.00588 7.68353 2.00588 7.29301 2.39641L4.46978 5.21963L3.93945 5.74996L5.00011 6.81062L5.53044 6.28029L8.00011 3.81062L10.4698 6.28029L11.0001 6.81062L12.0608 5.74996L11.5304 5.21963L8.70722 2.39641ZM5.53044 9.71963L5.00011 9.1893L3.93945 10.25L4.46978 10.7803L7.29301 13.6035C7.68353 13.994 8.3167 13.994 8.70722 13.6035L11.5304 10.7803L12.0608 10.25L11.0001 9.1893L10.4698 9.71963L8.00011 12.1893L5.53044 9.71963Z"
        fill="currentColor"
      />
    </svg>
  )
}

export const CALL_STACK_STYLES = `
  .error-overlay-call-stack-container {
    position: relative;
    margin-top: 8px;
  }

  .error-overlay-call-stack-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    min-height: var(--size-28);
    padding: 8px 8px 12px 4px;
    width: 100%;
  }

  .error-overlay-call-stack-title {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 8px;

    margin: 0;

    color: var(--color-gray-1000);
    font-size: var(--size-16);
    font-weight: 500;
  }

  .error-overlay-call-stack-count {
    display: flex;
    justify-content: center;
    align-items: center;

    width: var(--size-20);
    height: var(--size-20);
    gap: 4px;

    color: var(--color-gray-1000);
    text-align: center;
    font-size: var(--size-11);
    font-weight: 500;
    line-height: var(--size-16);

    border-radius: var(--rounded-full);
    background: var(--color-gray-300);
  }

  .error-overlay-call-stack-ignored-list-toggle-button {
    all: unset;
    display: flex;
    align-items: center;
    gap: 6px;
    color: var(--color-gray-900);
    font-size: var(--size-14);
    line-height: var(--size-20);
    border-radius: 6px;
    padding: 4px 6px;
    margin-right: -6px;
    transition: background 150ms ease;

    &:hover {
      background: var(--color-gray-100);
    }

    &:focus {
      outline: var(--focus-ring);
    }

    svg {
      width: var(--size-16);
      height: var(--size-16);
    }
  }
`
