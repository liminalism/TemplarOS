'use strict';
const vscode = require('vscode');
const { execFile } = require('node:child_process');

function activate(context) {
    const output = vscode.window.createOutputChannel('TemplarOS Garden');
    context.subscriptions.push(output);
    const report = () => {
        for (const folder of vscode.workspace.workspaceFolders || []) {
            // Remote extension hosts cannot prove the local editor's cgroup or directory.
            if (folder.uri.scheme !== 'file') continue;
            execFile('templar', ['garden', 'observe', 'project-entered', folder.uri.fsPath,
                '--role', 'code-editor'], { timeout: 5000, maxBuffer: 8192 }, (error) => {
                if (error) output.appendLine('Workspace attribution unavailable: verified local launcher instance/sessiond required.');
            });
        }
    };
    context.subscriptions.push(vscode.workspace.onDidChangeWorkspaceFolders(report));
    report();
}
module.exports = { activate };
