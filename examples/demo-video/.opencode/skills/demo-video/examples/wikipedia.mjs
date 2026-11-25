// Example scene script: searching Wikipedia (about 35 s).
//   node .opencode/skills/demo-video/render.mjs .opencode/skills/demo-video/examples/wikipedia.mjs
//
// A scene script is an ES module whose default export describes the video. Each scene's
// actions run while its narration plays; the scene then waits for the narration to end.

const ARTICLE = 'Rutherford Appleton Laboratory';

export default {
  title: 'Searching Wikipedia',

  // Opened before recording starts, so the video never shows a loading page.
  url: 'https://en.wikipedia.org/wiki/Main_Page',

  // CSS selectors hidden on every page (here: fundraising and site-notice banners).
  hide: ['#siteNotice', '#centralNotice', '.cn-fundraising', '.frb'],

  scenes: [
    {
      id: 'intro',
      card: { kicker: 'Wikipedia in 60 seconds', title: 'Searching Wikipedia', subtitle: 'Find an article, jump to a section, and preview a link' },
      narration: "In this short video, we'll search Wikipedia for an article, jump straight to one of its sections, and preview a link without leaving the page.",
    },
    {
      id: 'search',
      chapter: 'Search with suggestions',
      narration: 'The search box sits at the top of every page. As we start typing, Wikipedia suggests matching articles.',
      async actions({ page, pause, sentence }) {
        // Clicking the plain search box swaps in Wikipedia's JS search widget (the
        // original <input> is detached), so wait for the new input to take focus.
        await page.getByRole('searchbox', { name: /search wikipedia/i }).first().click();
        await page.waitForFunction(() => document.activeElement?.matches('input[type=search]'));
        await sentence(1); // "As we start typing..."
        await page.keyboard.type('Rutherford Appleton', { delay: 90 });
        await page.locator('.cdx-menu-item').filter({ hasText: ARTICLE }).first().waitFor();
      },
    },
    {
      id: 'open',
      chapter: 'Open the article',
      narration: `We pick the ${ARTICLE} from the list, and its article opens straight away.`,
      async actions({ page }) {
        await page.locator('.cdx-menu-item').filter({ hasText: ARTICLE }).first().click();
        await page.waitForURL(/Rutherford_Appleton_Laboratory/);
        await page.locator('#firstHeading').waitFor();
      },
    },
    {
      id: 'contents',
      chapter: 'Jump to a section',
      narration: 'Long articles have a contents list down the left. One click takes us straight to the History section.',
      hold: 1200, // ms to stay on screen after the narration ends (default 450)
      async actions({ page, pause, sentence }) {
        await sentence(1); // "One click takes us..."
        await page.locator('#vector-toc a.vector-toc-link[href="#History"]').click();
        await pause(600); // smooth scroll
        // Move the pointer off the sticky contents list, onto the section we landed on.
        await page.locator('#History').hover();
      },
    },
    {
      id: 'preview',
      chapter: 'Preview a link',
      narration: 'Hovering over a link, like Ernest Rutherford, shows a preview of that article, so you can check it without losing your place.',
      async actions({ page, pause }) {
        await pause(500);
        await page.locator('p').filter({ hasText: 'RAL is named after' }).getByRole('link', { name: 'Ernest Rutherford' }).hover();
        await page.locator('.mwe-popups').first().waitFor({ timeout: 8000 });
      },
    },
    {
      id: 'outro',
      card: { kicker: 'Recap', title: 'Type, pick, jump, preview', subtitle: "That's all it takes to find your way around Wikipedia" },
      narration: "And that's it: type in the search box, pick a suggestion, jump to a section, and hover over links to preview them as you read.",
    },
  ],
};
