import type { Room } from '../generated/protocol'
import { useServices } from '../context'
import { knownTags, tagName, userTag } from '../roomTags'
import { type Check, Picker } from './Picker'

/** Toggles a tag on `rooms` (`l`): Element's two, the user's own, or a new
 * one of the user's, named as typed. */
export function RoomTagPicker(props: { rooms: readonly Room[]; close: () => void }) {
  const { matrix } = useServices()
  const tags = knownTags(matrix.rooms.value ?? [], props.rooms)
  const check = (tag: string): Check => {
    const having = props.rooms.filter((r) => r.tags.includes(tag)).length
    return having === 0 ? 'none' : having === props.rooms.length ? 'all' : 'some'
  }
  const create = (filter: string) => {
    if (!filter.trim()) return null
    const tag = userTag(filter)
    return tags.includes(tag) ? null : tag
  }
  return (
    <Picker
      name="Tag"
      placeholder="Tag as…"
      filterName="Filter tags"
      listName="Tags"
      empty="No tags"
      items={tags}
      id={(tag) => tag}
      text={tagName}
      render={(tag) => (tags.includes(tag) ? tagName(tag) : `New tag: ${tagName(tag)}`)}
      check={(tag) => (tags.includes(tag) ? check(tag) : 'none')}
      create={create}
      choose={(tag) => matrix.toggleTag(props.rooms, tag)}
      close={props.close}
    />
  )
}
