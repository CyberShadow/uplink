import { cleanup, configure } from '@testing-library/preact'
import { afterEach } from 'vitest'

// jsdom does no layout.
Element.prototype.scrollIntoView = () => {}

// The fake server answers at once, but `nix flake check` runs these
// alongside builds and VM tests: a second (the default) is not always
// enough for a few renders on a loaded machine.
configure({ asyncUtilTimeout: 5000 })

afterEach(cleanup)
