import type { OverlayDispatch, OverlayState } from '../../shared'

import { Dialog, DialogContent, DialogHeader, DialogBody } from '../dialog'
import { Overlay } from '../overlay/overlay'
import { ACTION_DEVTOOLS_PANEL_TOGGLE } from '../../shared'
import { css } from '../../utils/css'

export function DevToolsPanel({
  // eslint-disable-next-line @typescript-eslint/no-unused-vars
  state,
  dispatch,
}: {
  state: OverlayState
  dispatch: OverlayDispatch
}) {
  const onClose = () => {
    dispatch({ type: ACTION_DEVTOOLS_PANEL_TOGGLE })
  }

  return (
    <Overlay data-nextjs-devtools-panel-overlay>
      <Dialog
        data-nextjs-devtools-panel-dialog
        aria-labelledby="nextjs__container_dev_tools_panel_label"
        aria-describedby="nextjs__container_dev_tools_panel_desc"
        onClose={onClose}
      >
        <DialogContent>
          <DialogHeader></DialogHeader>
          <DialogBody></DialogBody>
        </DialogContent>
      </Dialog>
    </Overlay>
  )
}

export const DEVTOOLS_PANEL_STYLES = css`
  [data-nextjs-devtools-panel-overlay] {
    padding: initial;
    top: 10vh;
  }

  [data-nextjs-devtools-panel-dialog] {
    -webkit-font-smoothing: antialiased;
    display: flex;
    flex-direction: column;
    background: var(--color-background-100);
    background-clip: padding-box;
    border: 1px solid var(--color-gray-400);
    border-radius: var(--rounded-xl);
    box-shadow: var(--shadow-lg);
    position: relative;
    overflow-y: auto;

    /* TODO: Remove once the content is filled. */
    min-width: 800px;
    min-height: 500px;

    /* This is handled from dialog/styles.ts */
    max-width: var(--next-dialog-max-width);
  }
`
