import { keyframes } from "@emotion/react";
import { useState } from "react";
import DeleteOutlinedIcon from "@mui/icons-material/DeleteOutlined";
import ExpandMoreIcon from "@mui/icons-material/ExpandMore";
import Box from "@mui/material/Box";
import Card from "@mui/material/Card";
import CardContent from "@mui/material/CardContent";
import Collapse from "@mui/material/Collapse";
import Divider from "@mui/material/Divider";
import IconButton from "@mui/material/IconButton";
import Stack from "@mui/material/Stack";
import Tooltip from "@mui/material/Tooltip";
import Typography from "@mui/material/Typography";
import { MAX_FRIENDLY_NAME_LENGTH, type Client, type DashboardConfig, type HardwareSpecs } from "../../api";
import { MISSING, formatCpu, formatDisk, formatGib, formatGpu, formatMemory } from "../../utils/format";
import { latestSample, metricsGpuCount, type History } from "../../hooks/useClients";
import ClientUsage from "./ClientUsage";
import InlineTextEdit from "../common/InlineTextEdit";

const timestampFormat = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "medium" });

function formatTimestamp(iso: string | null): string {
  if (!iso) return "Never";
  const date = new Date(iso);
  return Number.isNaN(date.getTime()) ? iso : timestampFormat.format(date);
}

function StatusDot({ connected }: { connected: boolean }) {
  const label = connected ? "Connected" : "Disconnected";
  return (
    <Box
      component="span"
      role="img"
      aria-label={label}
      title={label}
      sx={{
        width: 10,
        height: 10,
        borderRadius: "50%",
        flexShrink: 0,
        bgcolor: connected ? "success.main" : "error.main",
      }}
    />
  );
}

const BRIGHT_BLUE = "#2196f3";
const DIM_BLUE = "#0d2f52";

const blink = keyframes`
  0%, 100% { background-color: ${BRIGHT_BLUE}; }
  50% { background-color: ${DIM_BLUE}; }
`;

/**
 * While the client is working on a job, cycles bright blue / dim blue, one
 * full cycle per DASHBOARD_BLINK_PERIOD_MS. Idle, it's a steady dim blue.
 */
function JobDot({ jobs, periodMs }: { jobs: number; periodMs: number }) {
  const label = jobs === 0 ? "Idle" : jobs === 1 ? "Working on a job" : `Working on ${jobs} jobs`;
  return (
    <Box
      component="span"
      role="img"
      aria-label={label}
      title={label}
      sx={{
        width: 10,
        height: 10,
        borderRadius: "50%",
        flexShrink: 0,
        bgcolor: jobs > 0 ? BRIGHT_BLUE : DIM_BLUE,
        animation: jobs > 0 ? `${blink} ${periodMs}ms ease-in-out infinite` : "none",
        // Still shows it's working, just without the motion.
        "@media (prefers-reduced-motion: reduce)": { animation: "none" },
      }}
    />
  );
}

function DetailRow({ label, value, title }: { label: string; value: string; title?: string }) {
  return (
    <Stack
      direction="row"
      spacing={2}
      sx={{ justifyContent: "space-between" }}
    >
      <Typography
        variant="body2"
        color="text.secondary"
        sx={{ flexShrink: 0 }}
      >
        {label}
      </Typography>
      <Typography
        variant="body2"
        title={title}
        sx={{ textAlign: "right", minWidth: 0, overflowWrap: "anywhere" }}
      >
        {value}
      </Typography>
    </Stack>
  );
}

function Timestamp({ label, iso }: { label: string; iso: string | null }) {
  return (
    <DetailRow
      label={label}
      value={formatTimestamp(iso)}
      title={iso ?? undefined}
    />
  );
}

function Hardware({ specs }: { specs: HardwareSpecs | null }) {
  if (!specs) {
    return (
      <Typography
        variant="body2"
        color="text.secondary"
      >
        No hardware info yet
      </Typography>
    );
  }
  return (
    <>
      <DetailRow
        label="CPU"
        value={formatCpu(specs.cpu)}
        title={specs.cpu?.model ?? undefined}
      />
      <DetailRow
        label="RAM"
        value={formatMemory(specs.memory)}
      />
      {specs.gpus.length === 0 ? (
        <DetailRow
          label="GPU"
          value={MISSING}
        />
      ) : (
        specs.gpus.map((gpu, i) => (
          <DetailRow
            key={i}
            label="GPU"
            value={formatGpu(gpu)}
            title={gpu.model ?? undefined}
          />
        ))
      )}
      {specs.disks.length === 0 ? (
        <DetailRow
          label="Disk"
          value={MISSING}
        />
      ) : (
        specs.disks.map((disk, i) => (
          <DetailRow
            key={i}
            label="Disk"
            value={formatDisk(disk)}
          />
        ))
      )}
    </>
  );
}

/** Live swap use from the newest sample, e.g. "0.3 / 8.0 GiB". */
function formatSwap(history: History | undefined): string {
  const latest = latestSample(history);
  const total = latest?.swap_total_bytes;
  if (total == null) return MISSING;
  if (total === 0) return "None";
  return `${formatGib(latest?.swap_used_bytes ?? 0)} / ${formatGib(total)} GiB`;
}

interface Props {
  client: Client;
  /** Undefined while disconnected. */
  history: History | undefined;
  config: DashboardConfig;
  /** The signed-in user, for "Owner: you". */
  meId: string;
  onRename: (friendlyName: string) => Promise<void>;
  onDelete: () => void;
}

export default function ClientCard({ client, history, config, meId, onRename, onDelete }: Props) {
  const [expanded, setExpanded] = useState(false);
  const detailsId = `client-${client.client_id}-details`;
  const gpuCount = metricsGpuCount(history, client.specs);
  const collapsedGpus = Math.min(gpuCount, config.collapsed_max_gpus);
  const mine = client.owner.user_id === meId;
  const owner = `Owner: ${mine ? "you" : client.owner.username}`;

  return (
    <Card
      elevation={expanded ? 4 : 2}
      sx={{ transition: (theme) => theme.transitions.create("box-shadow") }}
    >
      <CardContent
        sx={{
          display: "flex",
          alignItems: "center",
          columnGap: 2,
          "&:last-child": { pb: 2 },
        }}
      >
        <Box
          sx={{
            flex: "1 1 auto",
            minWidth: 0,
            display: "flex",
            flexWrap: "wrap",
            alignItems: "center",
            columnGap: 2,
            rowGap: 1.5,
          }}
        >
          <Box sx={{ flex: "0 1 auto", minWidth: 0, display: "flex", alignItems: "center", columnGap: 1 }}>
            <StatusDot connected={client.connected} />
            <JobDot
              jobs={client.active_jobs}
              periodMs={config.blink_period_ms}
            />
            <Box sx={{ flex: "0 1 15rem", minWidth: 0, ml: 1 }}>
              {client.can_manage ? (
                <InlineTextEdit
                  value={client.friendly_name}
                  onSave={onRename}
                  label="Client name"
                  placeholder={`Blank resets to ${client.hostname}`}
                  maxLength={MAX_FRIENDLY_NAME_LENGTH}
                  typographyProps={{ variant: "body1", sx: { fontWeight: 500 } }}
                />
              ) : (
                <Typography
                  noWrap
                  variant="body1"
                  sx={{ fontWeight: 500 }}
                >
                  {client.friendly_name}
                </Typography>
              )}
              {/* Collapsed, only when it says something: everyone's own cards would all say "you". */}
              {!mine && (
                <Typography
                  variant="caption"
                  color="text.secondary"
                  noWrap
                  component="div"
                >
                  {owner}
                </Typography>
              )}
            </Box>
          </Box>
          <ClientUsage
            history={history}
            config={config}
            system
            gpuFrom={0}
            gpuTo={collapsedGpus}
            sx={{ flex: "1 1 auto" }}
          />
        </Box>
        {client.can_manage && (
          <Tooltip title="Delete client">
            <IconButton
              onClick={onDelete}
              aria-label={`Delete ${client.friendly_name}`}
              size="small"
            >
              <DeleteOutlinedIcon />
            </IconButton>
          </Tooltip>
        )}
        <IconButton
          onClick={() => setExpanded((v) => !v)}
          aria-expanded={expanded}
          aria-controls={detailsId}
          aria-label={expanded ? "Hide details" : "Show details"}
          size="small"
        >
          <ExpandMoreIcon
            sx={{
              transform: expanded ? "rotate(180deg)" : "none",
              transition: (theme) => theme.transitions.create("transform"),
            }}
          />
        </IconButton>
      </CardContent>
      <Collapse
        in={expanded}
        timeout="auto"
        unmountOnExit
        id={detailsId}
      >
        <CardContent sx={{ pt: 0, "&:last-child": { pb: 2 } }}>
          <Divider sx={{ my: 1.5 }} />
          {/* GPUs beyond the collapsed row's DASHBOARD_COLLAPSED_MAX_GPUS. */}
          <ClientUsage
            history={history}
            config={config}
            system={false}
            gpuFrom={collapsedGpus}
            gpuTo={gpuCount}
            sx={{ mb: 1.5 }}
          />
          <Stack spacing={0.5}>
            <Hardware specs={client.specs} />
            <DetailRow
              label="OS"
              value={client.os ?? MISSING}
            />
            <DetailRow
              label="Client version"
              value={client.client_version ?? MISSING}
            />
            <DetailRow
              label="Swap in use"
              value={formatSwap(history)}
            />
          </Stack>
          <Divider sx={{ my: 1.5 }} />
          <Stack spacing={0.5}>
            <DetailRow
              label="Owner"
              value={mine ? "You" : client.owner.username}
            />
            <DetailRow
              label="Hostname"
              value={client.hostname}
            />
            <Timestamp
              label="Enrolled"
              iso={client.enrolled_at}
            />
            <Timestamp
              label="Last seen"
              iso={client.last_seen_at}
            />
          </Stack>
        </CardContent>
      </Collapse>
    </Card>
  );
}
