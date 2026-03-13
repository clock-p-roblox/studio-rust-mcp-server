export type InsertModelArgs = {
	query: string,
}

export type RunCodeArgs = {
	command: string,
}

export type TestMode = "start_play" | "run_server"

export type StartStopPlayMode = TestMode | "stop"

export type StartStopPlayArgs = {
	mode: StartStopPlayMode,
}

export type LaunchStudioSessionArgs = {
	mode: TestMode,
}

export type RunScriptInPlayModeArgs = {
	code: string,
	timeout: number?,
	mode: TestMode,
}

export type ToolArgs =
	{}
	| { InsertModel: InsertModelArgs }
	| { RunCode: RunCodeArgs }
	| { StartStopPlay: StartStopPlayArgs }
	| { LaunchStudioSession: LaunchStudioSessionArgs }
	| { RunScriptInPlayMode: RunScriptInPlayModeArgs }

export type ToolFunction = (ToolArgs) -> string?

return {}
