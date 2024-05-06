import * as vscode from "vscode";
import { findDuplicantFile } from "./lib/find-duplicant-file";
import { Duplicant } from "./lib/duplicant-reader";
import path from "node:path";

async function promptForDuplicant(): Promise<Duplicant | undefined> {
	const path = await vscode.window.showOpenDialog({
		canSelectFiles: true,
		canSelectFolders: false,
		canSelectMany: false,
		openLabel: "Select Duplicant file",
		filters: {
			"Duplicant files": ["dupl", "dupl.bin", "bin"],
		},
	});

	if (!path) {
		return;
	}

	try {
		return await findDuplicantFile(path[0].fsPath);
	} catch (err) {
		vscode.window.showErrorMessage((<Error>err).message);
		return;
	}
}

export function activate(context: vscode.ExtensionContext) {
	let activeDuplicant: Duplicant | undefined;

	context.subscriptions.push(
		vscode.commands.registerCommand("duplicant.showheatmap", async () => {
			// Get the root folder of the workspace
			const rootPath = vscode.workspace.workspaceFolders?.[0].uri.fsPath;
			if (!rootPath) {
				vscode.window.showErrorMessage("No workspace folder found");
				return;
			}

			// Try to find the duplicant file
			let duplicant: Duplicant;
			try {
				duplicant = await findDuplicantFile(rootPath);

				const answer = await vscode.window.showInformationMessage(
					`Discovered '${duplicant.path}'; load it?`,
					"Yes",
					"Another file...",
				);
				if (answer !== "Yes") {
					let maybeDuplicant = await promptForDuplicant();

					if (!maybeDuplicant) {
						return;
					}

					duplicant = maybeDuplicant;
				}
			} catch (err) {
				const answer = await vscode.window.showInformationMessage(
					"Heatmap not discovered; do you want to search for it yourself?",
					"Yes",
					"No",
				);
				if (answer !== "Yes") {
					vscode.window.showErrorMessage((<Error>err).message);
					return;
				}

				let maybeDuplicant = await promptForDuplicant();

				if (!maybeDuplicant) {
					return;
				}

				duplicant = maybeDuplicant;
			}

			vscode.window.showInformationMessage(
				`Duplicant file found: ${duplicant.path}`,
			);
			activeDuplicant = duplicant;
		}),
	);

	context.subscriptions.push(
		vscode.commands.registerCommand(
			"duplicant.jumpto",
			(filepath: string, line: number) => {
				// Get the full file path
				const rootPath =
					vscode.workspace.workspaceFolders?.[0].uri.fsPath;
				if (!rootPath) {
					return;
				}

				const fullpath = path.join(rootPath, filepath);
				const pos1 = new vscode.Position(line - 1, 0);
				const openPath = vscode.Uri.file(fullpath);
				vscode.workspace.openTextDocument(openPath).then((doc) => {
					vscode.window.showTextDocument(doc).then((editor) => {
						// Line added - by having a selection at the same position twice, the cursor jumps there
						editor.selections = [new vscode.Selection(pos1, pos1)];

						// And the visible range jumps there too
						const range = new vscode.Range(pos1, pos1);
						editor.revealRange(range);
					});
				});
			},
		),
	);

	context.subscriptions.push(
		vscode.window.onDidChangeActiveTextEditor(async (editor) => {
			if (!editor || !activeDuplicant) {
				return;
			}

			// Get the filepath relative to the root of the workspace
			const rootPath = vscode.workspace.workspaceFolders?.[0].uri.fsPath;
			if (!rootPath) {
				return;
			}

			const relativePath = vscode.workspace.asRelativePath(
				editor.document.uri,
			);

			const pathReader = await activeDuplicant.findPath(relativePath);
			if (!pathReader) {
				return;
			}

			const duplicantLines = await pathReader.getLines();

			for (const line of duplicantLines) {
				// Add a decorator for each line
				const decoration = vscode.window.createTextEditorDecorationType(
					{
						backgroundColor: `rgba(255, 0, 0, ${0.05 + (line.similarityCount / activeDuplicant.maxSimilarityCount) * 0.9})`,
						isWholeLine: true,
					},
				);

				const similarities = await line.getSimilarities();

				const similarityStrings = [];
				for (const similarity of similarities) {
					const path = await similarity.getPath();
					const commandUri = vscode.Uri.parse(
						`command:duplicant.jumpto?${encodeURIComponent(JSON.stringify([path.path, similarity.lineNo]))}`,
					);
					similarityStrings.push(
						`- [\`${path.path}:${similarity.lineNo}\`](${commandUri})`,
					);
				}

				const markdown = new vscode.MarkdownString(
					`Similarity count: ${line.similarityCount}\n\n${similarityStrings.join("\n")}`,
				);
				markdown.isTrusted = true;

				const options = {
					range: new vscode.Range(
						line.lineNo - 1,
						0,
						line.lineNo - 1,
						editor.document.lineAt(line.lineNo - 1).text.length,
					),
					hoverMessage: markdown,
					renderOptions: {
						dark: {
							after: {
								contentText: `(${line.similarityCount} similarities)`,
								color: "rgba(255, 255, 255, 0.5)",
								margin: "0 0 0 10px",
							},
						},
						light: {
							after: {
								contentText: `(${line.similarityCount} similarities)`,
								color: "rgba(0, 0, 0, 0.5)",
								margin: "0 0 0 10px",
							},
						},
					},
				};

				editor.setDecorations(decoration, [options]);
			}
		}),
	);
}

export function deactivate() {}
