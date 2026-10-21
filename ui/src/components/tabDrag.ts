/**
 * The tab being dragged. Browsers hide a drag's data until the drop, so the drop targets read the
 * id from here while the pointer moves (to decide whether an edge would really split).
 */
let dragged: string | null = null;

export const setDraggedTab = (id: string | null) => {
  dragged = id;
};

export const draggedTab = () => dragged;
