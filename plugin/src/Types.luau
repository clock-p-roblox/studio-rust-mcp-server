export type InsertModelArgs = {
	query: string,
}

export type RunCodeArgs = {
	command: string,
	diagnostic: boolean?,
}

export type TestMode = "start_play" | "run_server"

export type StartStopPlayMode = TestMode | "stop"
export type StudioSessionState = "stop" | "starting_play" | "play" | "stopping"

export type StartStopPlayArgs = {
	mode: StartStopPlayMode,
}

export type LaunchStudioSessionArgs = {
	mode: TestMode,
}

export type ToolArgs =
	{}
	| { InsertModel: InsertModelArgs }
	| { RunCode: RunCodeArgs }
	| { GetStudioSessionState: {} }
	| { StartStopPlay: StartStopPlayArgs }
	| { LaunchStudioSession: LaunchStudioSessionArgs }

export type ToolFunction = (ToolArgs) -> string?

return {}
