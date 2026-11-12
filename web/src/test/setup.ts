import { cleanup, configure } from '@testing-library/preact'
import { afterEach } from 'vitest'
import './intersection'

// As index.html has it.
document.head.insertAdjacentHTML('beforeend', '<link rel="icon" type="image/svg+xml" href="/logo.svg" />')

// jsdom does no layout, nor media queries: the system's theme is light.
Element.prototype.scrollIntoView = () => {}
Element.prototype.scrollBy = () => {}
window.matchMedia = (query: string) =>
  ({
    matches: false,
    media: query,
    addEventListener: () => {},
    removeEventListener: () => {},
  }) as unknown as MediaQueryList

// The fake server answers at once, but `nix flake check` runs these
// alongside builds and VM tests: a second (the default) is not always
// enough for a few renders on a loaded machine.
configure({ asyncUtilTimeout: 5000 })

afterEach(cleanup)
