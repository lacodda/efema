// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

import address from './src/integrations/address.mjs';
import llms from './src/integrations/llms.mjs';

// Where the site is served, said once. A github.io project site lives under
// the repository's name; a custom domain serves from the root, with
// public/CNAME naming it. Moving to a domain is this line plus that file - and
// the address check fails the build on anything left pointing at the old one.
const home = new URL('https://lacodda.github.io/efema/');

// Said once as well: the site, llms.txt and the page metadata all read these.
const title = "efema";
const description = "A sync relay for local-first apps: it keeps changes in order and cannot read them";

export default defineConfig({
	site: home.origin,
	base: home.pathname,
	integrations: [
		address(),
		starlight({
			title,
			description,
			// The 404 page is a content page (src/content/docs/404.md): Starlight's
			// own route looks for that entry and warns on every build without it.
			disable404Route: true,
			logo: {
				src: './src/assets/logo.svg',
				alt: title,
			},
			favicon: '/favicon.svg',
			// The touch icon is the mark at level L - 180px is far above the size
			// where the S tile is the only thing that reads. Under the base path,
			// like every file in public/.
			head: [
				{ tag: 'link', attrs: { rel: 'apple-touch-icon', href: `${home.pathname}apple-touch-icon.png` } },
				{ tag: 'meta', attrs: { property: 'og:image', content: 'https://raw.githubusercontent.com/lacodda/efema/main/assets/social-preview.png' } },
				{ tag: 'meta', attrs: { name: 'twitter:card', content: 'summary_large_image' } },
			],
			customCss: ['./src/styles/brand.css'],
			social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/lacodda/efema' }],
			editLink: {
				baseUrl: 'https://github.com/lacodda/efema/edit/main/docs/',
			},
			sidebar: [
				{ label: 'Getting Started', slug: 'getting-started' },
				{
					label: 'Guides',
					items: [{ autogenerate: { directory: 'guides' } }],
				},
				{
					label: 'Concepts',
					items: [{ autogenerate: { directory: 'concepts' } }],
				},
				{
					label: 'Reference',
					items: [{ autogenerate: { directory: 'reference' } }],
				},
			],
		}),
		llms({ title, description }),
	],
});
