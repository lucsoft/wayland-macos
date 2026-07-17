// Bare nested KWin has no shell (plasmashell) to activate/manage windows, and it
// otherwise leaves the client window unpainted (black output). Activating and
// maximizing each window forces KWin to composite it into the output.
function show(w) {
    if (!w || w.specialWindow || !w.normalWindow) return;
    w.minimized = false;
    try { w.setMaximize(true, true); } catch (e) {}
    workspace.activeWindow = w;
}
var list = workspace.windowList ? workspace.windowList() : workspace.clientList();
for (var i = 0; i < list.length; i++) show(list[i]);
(workspace.windowAdded || workspace.clientAdded).connect(show);
