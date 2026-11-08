import { cleanup } from '@testing-library/preact'
import { afterEach } from 'vitest'

// jsdom does no layout.
Element.prototype.scrollIntoView = () => {}

afterEach(cleanup)
