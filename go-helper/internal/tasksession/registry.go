package tasksession

import (
	"errors"
	"fmt"
	"regexp"
	"sort"
	"sync"
	"time"
)

var placeIDPattern = regexp.MustCompile(`^[0-9]+$`)

const (
	CodeImmutableMismatch = "immutable_mismatch"
	CodeTaskEnded         = "task_ended"
	CodeTaskNotRegistered = "task_not_registered"
)

type DesiredStudioStore interface {
	EnsureTaskDesired(taskID string, placeID string)
	RemoveTaskDesired(taskID string)
}

type HeartbeatRequest struct {
	TaskID               string `json:"task_id"`
	MachineName          string `json:"machine_name"`
	PlaceID              string `json:"place_id"`
	TaskAgentPID         int    `json:"task_agent_pid"`
	TaskAgentStartedAtMS int64  `json:"task_agent_started_at_ms"`
	RojoUpstreamURL      string `json:"rojo_upstream_url"`
}

type HeartbeatResponse struct {
	OK           bool   `json:"ok"`
	TaskID       string `json:"task_id"`
	State        string `json:"state"`
	LeaseTimeout int64  `json:"lease_timeout_ms"`
}

type ReleaseResponse struct {
	OK         bool   `json:"ok"`
	TaskID     string `json:"task_id"`
	State      string `json:"state"`
	KilledPIDs []int  `json:"killed_pids"`
}

type ReleaseRequest struct {
	TaskAgentPID         int   `json:"task_agent_pid"`
	TaskAgentStartedAtMS int64 `json:"task_agent_started_at_ms"`
}

type StatusResponse struct {
	OK              bool       `json:"ok"`
	TaskID          string     `json:"task_id"`
	State           string     `json:"state"`
	Contract        *Contract  `json:"contract,omitempty"`
	LastHeartbeatAt *time.Time `json:"last_heartbeat_at,omitempty"`
	LeaseAgeMS      *int64     `json:"lease_age_ms,omitempty"`
	LeaseTimeoutMS  int64      `json:"lease_timeout_ms"`
}

type Summary struct {
	Live    []SessionSnapshot `json:"live"`
	Expired []SessionSnapshot `json:"expired"`
	Ended   []SessionSnapshot `json:"ended"`
}

type SessionSnapshot struct {
	TaskID          string     `json:"task_id"`
	State           string     `json:"state"`
	Contract        Contract   `json:"contract"`
	LastHeartbeatAt *time.Time `json:"last_heartbeat_at,omitempty"`
}

type Contract struct {
	TaskID               string `json:"task_id"`
	MachineName          string `json:"machine_name"`
	PlaceID              string `json:"place_id"`
	TaskAgentPID         int    `json:"task_agent_pid"`
	TaskAgentStartedAtMS int64  `json:"task_agent_started_at_ms"`
	RojoUpstreamURL      string `json:"rojo_upstream_url"`
}

type Session struct {
	Contract        Contract
	LastHeartbeatAt time.Time
}

type Registry struct {
	mu           sync.Mutex
	leaseTimeout time.Duration
	store        DesiredStudioStore
	now          func() time.Time
	live         map[string]Session
	expired      map[string]Contract
	ended        map[string]Contract
}

type Error struct {
	Code    string `json:"code"`
	Message string `json:"message"`
}

func (e *Error) Error() string {
	return e.Message
}

func NewRegistry(leaseTimeout time.Duration, store DesiredStudioStore) *Registry {
	return &Registry{
		leaseTimeout: leaseTimeout,
		store:        store,
		now:          time.Now,
		live:         make(map[string]Session),
		expired:      make(map[string]Contract),
		ended:        make(map[string]Contract),
	}
}

func (r *Registry) SetNowForTest(now func() time.Time) {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.now = now
}

func (r *Registry) Heartbeat(pathTaskID string, request HeartbeatRequest) (HeartbeatResponse, error) {
	contract, err := contractFromHeartbeat(pathTaskID, request)
	if err != nil {
		return HeartbeatResponse{}, err
	}

	r.mu.Lock()
	defer r.mu.Unlock()

	if _, ok := r.ended[contract.TaskID]; ok {
		return HeartbeatResponse{}, &Error{
			Code:    CodeTaskEnded,
			Message: fmt.Sprintf("task %s has been gracefully released", contract.TaskID),
		}
	}
	if live, ok := r.live[contract.TaskID]; ok {
		if err := requireSameContract(live.Contract, contract); err != nil {
			return HeartbeatResponse{}, err
		}
		live.LastHeartbeatAt = r.now()
		r.live[contract.TaskID] = live
		r.store.EnsureTaskDesired(contract.TaskID, contract.PlaceID)
		return HeartbeatResponse{
			OK:           true,
			TaskID:       contract.TaskID,
			State:        "live",
			LeaseTimeout: r.leaseTimeout.Milliseconds(),
		}, nil
	}
	if expired, ok := r.expired[contract.TaskID]; ok {
		if err := requireSameContract(expired, contract); err != nil {
			return HeartbeatResponse{}, err
		}
		delete(r.expired, contract.TaskID)
	}

	r.live[contract.TaskID] = Session{
		Contract:        contract,
		LastHeartbeatAt: r.now(),
	}
	r.store.EnsureTaskDesired(contract.TaskID, contract.PlaceID)
	return HeartbeatResponse{
		OK:           true,
		TaskID:       contract.TaskID,
		State:        "live",
		LeaseTimeout: r.leaseTimeout.Milliseconds(),
	}, nil
}

func (r *Registry) Release(taskID string, request ReleaseRequest) (ReleaseResponse, error) {
	if taskID == "" {
		return ReleaseResponse{}, errors.New("task_id must not be empty")
	}

	r.mu.Lock()
	session, live := r.live[taskID]
	expired, wasExpired := r.expired[taskID]
	if !live && !wasExpired {
		if _, ended := r.ended[taskID]; ended {
			r.mu.Unlock()
			return ReleaseResponse{
				OK:         true,
				TaskID:     taskID,
				State:      "ended",
				KilledPIDs: []int{},
			}, nil
		}
		r.mu.Unlock()
		return ReleaseResponse{}, &Error{
			Code:    CodeTaskNotRegistered,
			Message: fmt.Sprintf("helper2 has no registered session for task_id %s", taskID),
		}
	}
	contract := expired
	if live {
		contract = session.Contract
	}
	if request.TaskAgentPID <= 0 || request.TaskAgentStartedAtMS <= 0 {
		r.mu.Unlock()
		return ReleaseResponse{}, errors.New("task_agent_pid and task_agent_started_at_ms are required for release")
	}
	if request.TaskAgentPID != contract.TaskAgentPID || request.TaskAgentStartedAtMS != contract.TaskAgentStartedAtMS {
		r.mu.Unlock()
		return ReleaseResponse{}, &Error{
			Code:    CodeImmutableMismatch,
			Message: fmt.Sprintf("release identity does not match task_id %s", taskID),
		}
	}
	delete(r.live, taskID)
	delete(r.expired, taskID)
	if live {
		r.ended[taskID] = session.Contract
	} else {
		r.ended[taskID] = expired
	}
	r.store.RemoveTaskDesired(taskID)
	r.mu.Unlock()

	return ReleaseResponse{
		OK:         true,
		TaskID:     taskID,
		State:      "ended",
		KilledPIDs: []int{},
	}, nil
}

func (r *Registry) Status(taskID string) StatusResponse {
	r.mu.Lock()
	defer r.mu.Unlock()

	if session, ok := r.live[taskID]; ok {
		heartbeatAt := session.LastHeartbeatAt
		age := r.now().Sub(session.LastHeartbeatAt).Milliseconds()
		contract := session.Contract
		return StatusResponse{
			OK:              true,
			TaskID:          taskID,
			State:           "live",
			Contract:        &contract,
			LastHeartbeatAt: &heartbeatAt,
			LeaseAgeMS:      &age,
			LeaseTimeoutMS:  r.leaseTimeout.Milliseconds(),
		}
	}
	if contract, ok := r.expired[taskID]; ok {
		return StatusResponse{
			OK:             true,
			TaskID:         taskID,
			State:          "expired",
			Contract:       &contract,
			LeaseTimeoutMS: r.leaseTimeout.Milliseconds(),
		}
	}
	if contract, ok := r.ended[taskID]; ok {
		return StatusResponse{
			OK:             true,
			TaskID:         taskID,
			State:          "ended",
			Contract:       &contract,
			LeaseTimeoutMS: r.leaseTimeout.Milliseconds(),
		}
	}
	return StatusResponse{
		OK:             false,
		TaskID:         taskID,
		State:          "not_registered",
		LeaseTimeoutMS: r.leaseTimeout.Milliseconds(),
	}
}

func (r *Registry) ExpireStale() []string {
	r.mu.Lock()
	now := r.now()
	expiredTaskIDs := make([]string, 0)
	for taskID, session := range r.live {
		if now.Sub(session.LastHeartbeatAt) <= r.leaseTimeout {
			continue
		}
		delete(r.live, taskID)
		r.expired[taskID] = session.Contract
		expiredTaskIDs = append(expiredTaskIDs, taskID)
		r.store.RemoveTaskDesired(taskID)
	}
	r.mu.Unlock()

	return expiredTaskIDs
}

func (r *Registry) Summary() Summary {
	r.mu.Lock()
	defer r.mu.Unlock()

	summary := Summary{
		Live:    make([]SessionSnapshot, 0, len(r.live)),
		Expired: make([]SessionSnapshot, 0, len(r.expired)),
		Ended:   make([]SessionSnapshot, 0, len(r.ended)),
	}
	for taskID, session := range r.live {
		heartbeatAt := session.LastHeartbeatAt
		summary.Live = append(summary.Live, SessionSnapshot{
			TaskID:          taskID,
			State:           "live",
			Contract:        session.Contract,
			LastHeartbeatAt: &heartbeatAt,
		})
	}
	for taskID, contract := range r.expired {
		summary.Expired = append(summary.Expired, SessionSnapshot{
			TaskID:   taskID,
			State:    "expired",
			Contract: contract,
		})
	}
	for taskID, contract := range r.ended {
		summary.Ended = append(summary.Ended, SessionSnapshot{
			TaskID:   taskID,
			State:    "ended",
			Contract: contract,
		})
	}
	sortSnapshots(summary.Live)
	sortSnapshots(summary.Expired)
	sortSnapshots(summary.Ended)
	return summary
}

func contractFromHeartbeat(pathTaskID string, request HeartbeatRequest) (Contract, error) {
	taskID := request.TaskID
	if taskID == "" {
		taskID = pathTaskID
	}
	if taskID == "" {
		return Contract{}, errors.New("task_id must not be empty")
	}
	if pathTaskID != "" && request.TaskID != "" && pathTaskID != request.TaskID {
		return Contract{}, fmt.Errorf("path task_id %s does not match body task_id %s", pathTaskID, request.TaskID)
	}
	if request.MachineName == "" {
		return Contract{}, errors.New("machine_name must not be empty")
	}
	if request.PlaceID == "" {
		return Contract{}, errors.New("place_id must not be empty")
	}
	if !placeIDPattern.MatchString(request.PlaceID) {
		return Contract{}, errors.New("place_id must contain digits only")
	}
	if request.TaskAgentPID <= 0 {
		return Contract{}, errors.New("task_agent_pid must be positive")
	}
	if request.TaskAgentStartedAtMS <= 0 {
		return Contract{}, errors.New("task_agent_started_at_ms must be positive")
	}
	if request.RojoUpstreamURL == "" {
		return Contract{}, errors.New("rojo_upstream_url must not be empty")
	}
	return Contract{
		TaskID:               taskID,
		MachineName:          request.MachineName,
		PlaceID:              request.PlaceID,
		TaskAgentPID:         request.TaskAgentPID,
		TaskAgentStartedAtMS: request.TaskAgentStartedAtMS,
		RojoUpstreamURL:      request.RojoUpstreamURL,
	}, nil
}

func requireSameContract(existing Contract, incoming Contract) error {
	if existing == incoming {
		return nil
	}
	return &Error{
		Code: CodeImmutableMismatch,
		Message: fmt.Sprintf(
			"immutable task fields changed for task_id %s",
			existing.TaskID,
		),
	}
}

func sortSnapshots(values []SessionSnapshot) {
	sort.Slice(values, func(i, j int) bool {
		return values[i].TaskID < values[j].TaskID
	})
}
