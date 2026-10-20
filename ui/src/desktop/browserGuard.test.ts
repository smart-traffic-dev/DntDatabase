import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { installBrowserGuard } from './browserGuard';

let uninstall: () => void;
let host: HTMLDivElement;

beforeEach(() => {
  uninstall = installBrowserGuard(window);
  host = document.createElement('div');
  host.innerHTML = `
    <div class="chrome"><button type="button" id="btn">B</button><a id="link" href="https://example.com">x</a><img id="img" alt="" /></div>
    <input id="field" />
    <textarea id="area"></textarea>
    <div contenteditable="true" id="rich"></div>
    <div class="monaco-editor"><textarea id="monaco"></textarea></div>
  `;
  document.body.appendChild(host);
});

afterEach(() => {
  uninstall();
  host.remove();
});

const el = (id: string) => document.getElementById(id) as HTMLElement;

function key(target: Element, k: string, init: KeyboardEventInit = {}) {
  const e = new KeyboardEvent('keydown', { key: k, bubbles: true, cancelable: true, ...init });
  target.dispatchEvent(e);
  return e;
}

describe('browser keys', () => {
  it.each([
    ['F5', {}],
    ['F5', { ctrlKey: true }],
    ['r', { ctrlKey: true }],
    ['R', { ctrlKey: true, shiftKey: true }],
    ['F12', {}],
    ['I', { ctrlKey: true, shiftKey: true }],
    ['J', { ctrlKey: true, shiftKey: true }],
    ['C', { ctrlKey: true, shiftKey: true }],
    ['u', { ctrlKey: true }],
    ['p', { ctrlKey: true }],
    ['s', { ctrlKey: true }],
    ['o', { ctrlKey: true }],
    ['f', { ctrlKey: true }],
    ['g', { ctrlKey: true }],
    ['F3', {}],
    ['n', { ctrlKey: true }],
    ['N', { ctrlKey: true, shiftKey: true }],
    ['t', { ctrlKey: true }],
    ['w', { ctrlKey: true }],
    ['h', { ctrlKey: true }],
    ['j', { ctrlKey: true }],
    ['=', { ctrlKey: true }],
    ['+', { ctrlKey: true, shiftKey: true }],
    ['-', { ctrlKey: true }],
    ['0', { ctrlKey: true }],
    ['ArrowLeft', { altKey: true }],
    ['ArrowRight', { altKey: true }],
    ['BrowserBack', {}],
    ['BrowserForward', {}],
    ['BrowserRefresh', {}],
    ['F7', {}],
    ['Backspace', {}],
    ['a', { ctrlKey: true }],
  ])('blocks %s %o on the app chrome', (k, init) => {
    expect(key(el('btn'), k, init).defaultPrevented).toBe(true);
  });

  it.each([
    ['a', {}],
    ['Enter', {}],
    ['c', { ctrlKey: true }],
    ['v', { ctrlKey: true }],
    ['x', { ctrlKey: true }],
    ['z', { ctrlKey: true }],
    ['Tab', {}],
    ['F2', {}],
    ['F4', { altKey: true }],
  ])('leaves %s %o alone', (k, init) => {
    expect(key(el('btn'), k, init).defaultPrevented).toBe(false);
  });

  it('keeps editing keys in fields', () => {
    for (const id of ['field', 'area', 'rich']) {
      expect(key(el(id), 'Backspace').defaultPrevented).toBe(false);
      expect(key(el(id), 'a', { ctrlKey: true }).defaultPrevented).toBe(false);
    }
  });

  it('still blocks reload, print and zoom inside fields and the editor', () => {
    for (const id of ['field', 'monaco']) {
      expect(key(el(id), 'r', { ctrlKey: true }).defaultPrevented).toBe(true);
      expect(key(el(id), 'F5').defaultPrevented).toBe(true);
      expect(key(el(id), 'p', { ctrlKey: true }).defaultPrevented).toBe(true);
      expect(key(el(id), '=', { ctrlKey: true }).defaultPrevented).toBe(true);
    }
  });

  it('leaves find and the editor keys to Monaco', () => {
    for (const [k, init] of [
      ['f', { ctrlKey: true }],
      ['h', { ctrlKey: true }],
      ['g', { ctrlKey: true }],
      ['F3', {}],
      ['F3', { shiftKey: true }],
      ['d', { ctrlKey: true }],
      ['l', { ctrlKey: true }],
      ['u', { ctrlKey: true }],
      ['a', { ctrlKey: true }],
    ] as const) {
      expect(key(el('monaco'), k, init).defaultPrevented, `${k} ${JSON.stringify(init)}`).toBe(false);
    }
  });

  it('does not stop the app shortcuts from running', () => {
    const seen = vi.fn();
    window.addEventListener('keydown', seen, true);
    document.addEventListener('keydown', seen);
    try {
      key(el('btn'), 't', { ctrlKey: true });
      key(el('monaco'), 's', { ctrlKey: true });
      key(el('btn'), 'w', { ctrlKey: true });
    } finally {
      window.removeEventListener('keydown', seen, true);
      document.removeEventListener('keydown', seen);
    }
    expect(seen).toHaveBeenCalledTimes(6);
  });

  it('uninstall removes every listener', () => {
    uninstall();
    expect(key(el('btn'), 'F5').defaultPrevented).toBe(false);
    uninstall = installBrowserGuard(window);
  });
});

describe('context menu', () => {
  const menu = (target: Element) => {
    const e = new MouseEvent('contextmenu', { bubbles: true, cancelable: true });
    target.dispatchEvent(e);
    return e;
  };

  it('suppresses the browser menu on the chrome', () => {
    expect(menu(el('btn')).defaultPrevented).toBe(true);
    expect(menu(document.body).defaultPrevented).toBe(true);
  });

  it('keeps the native menu (copy/paste) in fields', () => {
    expect(menu(el('field')).defaultPrevented).toBe(false);
    expect(menu(el('area')).defaultPrevented).toBe(false);
    expect(menu(el('rich')).defaultPrevented).toBe(false);
  });

  it('runs after the app menus, which still see a fresh event', () => {
    const seen: boolean[] = [];
    const own = (e: Event) => seen.push(e.defaultPrevented);
    el('btn').addEventListener('contextmenu', own);
    menu(el('btn'));
    el('btn').removeEventListener('contextmenu', own);
    expect(seen).toEqual([false]);
  });
});

describe('mouse', () => {
  const mouse = (type: string, target: Element, init: MouseEventInit) => {
    const e = new MouseEvent(type, { bubbles: true, cancelable: true, ...init });
    target.dispatchEvent(e);
    return e;
  };

  it('blocks the back / forward mouse buttons', () => {
    for (const button of [3, 4]) {
      expect(mouse('mousedown', el('btn'), { button }).defaultPrevented).toBe(true);
      expect(mouse('mouseup', el('btn'), { button }).defaultPrevented).toBe(true);
      expect(mouse('auxclick', el('btn'), { button }).defaultPrevented).toBe(true);
    }
  });

  it('blocks middle-click autoscroll but keeps the auxclick for the app (middle-click closes a tab)', () => {
    expect(mouse('mousedown', el('btn'), { button: 1 }).defaultPrevented).toBe(true);
    const own = vi.fn();
    el('btn').addEventListener('auxclick', own);
    mouse('auxclick', el('btn'), { button: 1 });
    el('btn').removeEventListener('auxclick', own);
    expect(own).toHaveBeenCalledOnce();
  });

  it('does not open links in a new window', () => {
    expect(mouse('click', el('link'), { ctrlKey: true }).defaultPrevented).toBe(true);
    expect(mouse('click', el('link'), { shiftKey: true }).defaultPrevented).toBe(true);
    expect(mouse('auxclick', el('link'), { button: 1 }).defaultPrevented).toBe(true);
    expect(mouse('click', el('btn'), { ctrlKey: true }).defaultPrevented).toBe(false);
  });

  it('blocks Ctrl+wheel zoom only', () => {
    const wheel = (init: WheelEventInit) => {
      const e = new WheelEvent('wheel', { bubbles: true, cancelable: true, deltaY: 100, ...init });
      el('btn').dispatchEvent(e);
      return e;
    };
    expect(wheel({ ctrlKey: true }).defaultPrevented).toBe(true);
    expect(wheel({}).defaultPrevented).toBe(false);
  });
});

describe('drag and drop', () => {
  const drag = (type: string, target: Element) => {
    const e = new Event(type, { bubbles: true, cancelable: true });
    target.dispatchEvent(e);
    return e;
  };

  it('a drop outside a drop zone never navigates the webview', () => {
    expect(drag('dragover', el('btn')).defaultPrevented).toBe(true);
    expect(drag('drop', el('btn')).defaultPrevented).toBe(true);
  });

  it('app drop zones still get the drop', () => {
    const own = vi.fn();
    el('btn').addEventListener('drop', own);
    drag('drop', el('btn'));
    el('btn').removeEventListener('drop', own);
    expect(own).toHaveBeenCalledOnce();
  });

  it('a drop zone that accepts the drag keeps its dragover', () => {
    const accept = (e: Event) => e.preventDefault();
    el('btn').addEventListener('dragover', accept);
    const e = drag('dragover', el('btn'));
    el('btn').removeEventListener('dragover', accept);
    expect(e.defaultPrevented).toBe(true);
  });

  it('text can still be dropped into fields', () => {
    expect(drag('dragover', el('field')).defaultPrevented).toBe(false);
    expect(drag('drop', el('field')).defaultPrevented).toBe(false);
  });

  it('links and images are not draggable', () => {
    expect(drag('dragstart', el('link')).defaultPrevented).toBe(true);
    expect(drag('dragstart', el('img')).defaultPrevented).toBe(true);
    expect(drag('dragstart', el('btn')).defaultPrevented).toBe(false);
  });
});
