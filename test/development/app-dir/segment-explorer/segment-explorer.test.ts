import { nextTestSetup } from 'e2e-utils'
import { openDevToolsIndicatorPopover, retry } from 'next-test-utils'
import { Playwright } from 'next-webdriver'

async function getSegmentExplorerContent(browser: Playwright) {
  // open the devtool button
  await openDevToolsIndicatorPopover(browser)

  // open the segment explorer
  await browser.elementByCss('[data-segment-explorer]').click()

  //  wait for the segment explorer to be visible
  await browser.waitForElementByCss('[data-nextjs-devtool-segment-explorer]')

  const content = await browser.elementByCss(
    '[data-nextjs-devtool-segment-explorer]'
  )
  return content.text()
}

describe('segment-explorer', () => {
  const { next } = nextTestSetup({
    files: __dirname,
  })

  it('should render the segment explorer for parallel routes', async () => {
    const browser = await next.browser('/parallel-routes')
    expect(await getSegmentExplorerContent(browser)).toMatchInlineSnapshot(`
     "app/
     layout.tsx
     parallel-routes/
     layout.tsx
     page.tsx
     @bar/
     layout.tsx
     page.tsx
     @foo/
     layout.tsx
     page.tsx"
    `)
  })

  it('should render the segment explorer for nested routes', async () => {
    const browser = await next.browser('/blog/~/grid')
    expect(await getSegmentExplorerContent(browser)).toMatchInlineSnapshot(`
     "app/
     layout.tsx
     (v2)/
     layout.tsx
     blog / (team)/
     layout.tsx
     template.tsx
     ~ / (overview)/
     layout.tsx
     grid/
     page.tsx"
    `)
  })

  it('should cleanup on soft navigation', async () => {
    const browser = await next.browser('/soft-navigation/a')
    expect(await getSegmentExplorerContent(browser)).toMatchInlineSnapshot(`
     "app/
     layout.tsx
     soft-navigation / a/
     page.tsx"
    `)

    await browser.elementByCss('[href="/soft-navigation/b"]').click()
    await retry(async () => {
      expect(await browser.elementByCss('body').text()).toContain('Page B')
    })

    expect(await getSegmentExplorerContent(browser)).toMatchInlineSnapshot(`
     "app/
     layout.tsx
     soft-navigation / b/
     page.tsx"
    `)
  })

  it('should handle show file segments in order', async () => {
    const browser = await next.browser('/file-segments')
    expect(await getSegmentExplorerContent(browser)).toMatchInlineSnapshot(`
     "app/
     layout.tsx
     (all) / file-segments/
     layout.tsx
     template.tsx
     page.tsx"
    `)
  })
})
