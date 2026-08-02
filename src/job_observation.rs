use std::fmt;

const TOKEN_PREFIX: &str = "wjob1";
pub(crate) const MAX_JOB_OBSERVATION_TOKEN_LEN: usize = 192;
const MAX_JOB_ID_LEN: usize = 80;
const MAX_EPOCH_LEN: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobObservationExecutor {
    Agent,
    Local,
}

impl JobObservationExecutor {
    fn code(self) -> &'static str {
        match self {
            Self::Agent => "a",
            Self::Local => "l",
        }
    }

    fn parse(value: &str) -> Result<Self, JobObservationTokenError> {
        match value {
            "a" => Ok(Self::Agent),
            "l" => Ok(Self::Local),
            _ => Err(JobObservationTokenError::Malformed),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JobObservationToken {
    pub(crate) executor: JobObservationExecutor,
    pub(crate) job_id: String,
    pub(crate) epoch: String,
    pub(crate) revision: u64,
}

impl JobObservationToken {
    pub(crate) fn new(
        executor: JobObservationExecutor,
        job_id: impl Into<String>,
        epoch: impl Into<String>,
        revision: u64,
    ) -> Result<Self, JobObservationTokenError> {
        let token = Self {
            executor,
            job_id: job_id.into(),
            epoch: epoch.into(),
            revision,
        };
        validate_component(&token.job_id, MAX_JOB_ID_LEN)?;
        validate_component(&token.epoch, MAX_EPOCH_LEN)?;
        if token.encode().len() > MAX_JOB_OBSERVATION_TOKEN_LEN {
            return Err(JobObservationTokenError::Oversized);
        }
        Ok(token)
    }

    pub(crate) fn parse(value: &str) -> Result<Self, JobObservationTokenError> {
        if value.is_empty() || !value.is_ascii() {
            return Err(JobObservationTokenError::Malformed);
        }
        if value.len() > MAX_JOB_OBSERVATION_TOKEN_LEN {
            return Err(JobObservationTokenError::Oversized);
        }
        let mut parts = value.split(':');
        if parts.next() != Some(TOKEN_PREFIX) {
            return Err(JobObservationTokenError::Malformed);
        }
        let executor = JobObservationExecutor::parse(
            parts.next().ok_or(JobObservationTokenError::Malformed)?,
        )?;
        let job_id = parts
            .next()
            .ok_or(JobObservationTokenError::Malformed)?
            .to_string();
        let epoch = parts
            .next()
            .ok_or(JobObservationTokenError::Malformed)?
            .to_string();
        let revision_text = parts.next().ok_or(JobObservationTokenError::Malformed)?;
        if parts.next().is_some()
            || revision_text.is_empty()
            || (revision_text.len() > 1 && revision_text.starts_with('0'))
            || !revision_text.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(JobObservationTokenError::Malformed);
        }
        validate_component(&job_id, MAX_JOB_ID_LEN)?;
        validate_component(&epoch, MAX_EPOCH_LEN)?;
        let revision = revision_text
            .parse::<u64>()
            .map_err(|_| JobObservationTokenError::Malformed)?;
        let token = Self {
            executor,
            job_id,
            epoch,
            revision,
        };
        if token.encode() != value {
            return Err(JobObservationTokenError::Malformed);
        }
        Ok(token)
    }

    pub(crate) fn parse_bound(
        value: &str,
        executor: JobObservationExecutor,
        job_id: &str,
    ) -> Result<Self, JobObservationTokenError> {
        let token = Self::parse(value)?;
        if token.executor != executor {
            return Err(JobObservationTokenError::WrongExecutor);
        }
        if token.job_id != job_id {
            return Err(JobObservationTokenError::WrongJob);
        }
        Ok(token)
    }

    pub(crate) fn encode(&self) -> String {
        format!(
            "{TOKEN_PREFIX}:{}:{}:{}:{}",
            self.executor.code(),
            self.job_id,
            self.epoch,
            self.revision
        )
    }
}

fn validate_component(value: &str, max_len: usize) -> Result<(), JobObservationTokenError> {
    if value.is_empty() || value.len() > max_len {
        return Err(JobObservationTokenError::Malformed);
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(JobObservationTokenError::Malformed);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobObservationTokenError {
    Malformed,
    Oversized,
    WrongExecutor,
    WrongJob,
}

impl fmt::Display for JobObservationTokenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Malformed => "invalid after_observation_token: malformed opaque Job token",
            Self::Oversized => "invalid after_observation_token: token exceeds 192 bytes",
            Self::WrongExecutor => {
                "invalid after_observation_token: token belongs to a different executor"
            }
            Self::WrongJob => "invalid after_observation_token: token belongs to a different Job",
        })
    }
}

pub(crate) fn new_epoch() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observation_token_round_trips_canonically() {
        let token = JobObservationToken::new(
            JobObservationExecutor::Agent,
            "11111111-2222-3333-4444-555555555555",
            "0123456789abcdef0123456789abcdef",
            42,
        )
        .unwrap();
        let encoded = token.encode();
        assert!(encoded.len() <= MAX_JOB_OBSERVATION_TOKEN_LEN);
        assert_eq!(JobObservationToken::parse(&encoded).unwrap(), token);
    }

    #[test]
    fn observation_token_rejects_wrong_job_and_malformed_values() {
        let encoded = JobObservationToken::new(
            JobObservationExecutor::Local,
            "job-one",
            "0123456789abcdef",
            1,
        )
        .unwrap()
        .encode();
        assert_eq!(
            JobObservationToken::parse_bound(&encoded, JobObservationExecutor::Local, "job-two"),
            Err(JobObservationTokenError::WrongJob)
        );
        assert_eq!(
            JobObservationToken::parse("wjob1:l:job:epoch:01"),
            Err(JobObservationTokenError::Malformed)
        );
        assert_eq!(
            JobObservationToken::parse(&"x".repeat(MAX_JOB_OBSERVATION_TOKEN_LEN + 1)),
            Err(JobObservationTokenError::Oversized)
        );
    }
}
