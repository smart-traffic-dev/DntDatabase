import { create } from 'zustand';

/** What is being dragged over the editor area: a tab, or an object from the connection tree. */
export type DragItem = { kind: 'tab'; id: string } | { kind: 'object'; nodeId: string };

/** Drag payload types (the browser hides the data itself until the drop). */
export const TAB_DRAG_TYPE = 'text/x-dnt-tab';
export const OBJECT_DRAG_TYPE = 'text/x-dnt-object';

/**
 * The drag in progress. While it is set every editor group shows its drop layer above the editor
 * and the grid, so Monaco and the canvas grid never receive the drag.
 */
export const useDrag = create<{ item: DragItem | null }>()(() => ({ item: null }));

export const setDraggedTab = (id: string | null) => useDrag.setState({ item: id ? { kind: 'tab', id } : null });
export const setDraggedObject = (nodeId: string | null) => useDrag.setState({ item: nodeId ? { kind: 'object', nodeId } : null });
export const draggedItem = () => useDrag.getState().item;
/** The tab being dragged, if a tab is. */
export const draggedTab = () => {
  const item = useDrag.getState().item;
  return item?.kind === 'tab' ? item.id : null;
};
