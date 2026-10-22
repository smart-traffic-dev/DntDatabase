/** The app's keyboard shortcuts, listed on the Settings dialog's Shortcuts tab. */
export const SHORTCUTS: { group: string; items: [string, string][] }[] = [
  {
    group: 'Query',
    items: [
      ['Run the statement at the cursor', 'Ctrl+Enter / F9'],
      ['Run the selection, or all statements', 'Ctrl+R'],
      ['Run all statements', 'Ctrl+Shift+Enter'],
      ['Run the selection', 'Ctrl+Shift+R'],
      ['Explain', 'Ctrl+Shift+E'],
      ['Stop the running query', 'Esc'],
    ],
  },
  {
    group: 'Editor',
    items: [
      ['Format SQL', 'Ctrl+Shift+F'],
      ['Toggle comment', 'Ctrl+/'],
      ['Duplicate line', 'Ctrl+D'],
      ['Move line up / down', 'Alt+Up / Alt+Down'],
      ['Upper / lower case', 'Ctrl+Shift+U / Ctrl+U'],
      ['Open the table under the cursor (data / DDL)', 'F12 / Shift+F12'],
      ['Suggestions and snippets (sel, ins, upd, …)', 'Ctrl+Space'],
    ],
  },
  {
    group: 'Tabs and files',
    items: [
      ['New query', 'Ctrl+T'],
      ['Open SQL file', 'Ctrl+O'],
      ['Save', 'Ctrl+S'],
      ['Save as', 'Ctrl+Shift+S'],
      ['Close tab', 'Ctrl+W'],
      ['Next / previous tab', 'Ctrl+Tab / Ctrl+Shift+Tab'],
      ['Go to tab 1 to 8 / the last tab', 'Ctrl+1 … 8 / Ctrl+9'],
      ['Reopen the last closed tab', 'Ctrl+Shift+T'],
    ],
  },
  {
    group: 'Navigation',
    items: [
      ['Command palette (every menu action)', 'Ctrl+Shift+P'],
      ['Go to table, view or routine (type > for commands)', 'Ctrl+P'],
      ['Quick open: open the object on its DDL', 'Ctrl+Enter'],
      ['Focus the connection tree', 'Ctrl+E'],
    ],
  },
  {
    group: 'Connection tree',
    items: [
      ['Open (table or view: Data; connection or database: expand)', 'Enter'],
      ['Open the object on its DDL', 'Ctrl+Enter'],
      ['New query on the selected node', 'Ctrl+Q'],
      ['Rename / edit a connection or group', 'F2'],
      ['Refresh the selected node', 'F5'],
      ['Collapse / go to parent, expand / go to first child', 'Left / Right'],
      ['Type to filter the tree', 'any letter or digit'],
    ],
  },
  {
    group: 'Window',
    items: [
      ['Show / hide the sidebar', 'Ctrl+B'],
      ['Show / hide the right sidebar', 'Ctrl+Alt+B'],
      ['Open History and search it', 'Ctrl+Shift+H'],
      ['Show / hide the result panel (focused group)', 'Ctrl+J'],
      ['Maximize / restore the result panel', 'Ctrl+Shift+J'],
      ['Editor layout: single / columns / rows / grid', 'Ctrl+K 1 … 4'],
      ['New connection', 'Ctrl+Shift+N'],
      ['Settings', 'Ctrl+,'],
      ['Keyboard shortcuts', 'F1'],
      ['Code font bigger / smaller / reset', 'Ctrl+= / Ctrl+- / Ctrl+0'],
      ['Developer tools', 'Ctrl+Shift+I'],
      ['Open a menu', 'Alt+F, E, V, Q, A, T, H'],
    ],
  },
];
