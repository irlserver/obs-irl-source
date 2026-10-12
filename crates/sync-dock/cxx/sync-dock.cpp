/*
 * obs-irl-source — IRL streaming source plugin for OBS
 * https://irlserver.com
 *
 * Copyright (C) 2026 Thomas Lekanger
 * Copyright (C) 2026 Spillmaker
 * SPDX-License-Identifier: AGPL-3.0-or-later
 *
 * sync-dock.cpp — the IRL Sync dock.
 *
 * Spillmaker's dock from the C plugin (Spillmaker/obs-irl-source
 * `nal-timecodes`, src/sync/sync-dock.cpp), carried over into the Rust plugin
 * as the one piece of C++ it contains: the only part that needs Qt, and the
 * only part libobs's frontend API can host. Everything it reads and writes
 * goes through the function table in irl-sync-dock.h, which the `sync-dock`
 * crate fills from Rust at registration; the widget itself is unchanged apart
 * from that plumbing.
 *
 * It reads the registry and writes the three global settings; it never
 * touches a source's internals, so nothing here can race the media path.
 *
 * obs-frontend-api is resolved at runtime rather than linked, and no libobs
 * header is included: the two symbols needed here are already in the process
 * by the time any plugin loads, and resolving them dynamically means a
 * headless OBS simply gets no dock instead of failing to load the module —
 * the same bargain the websocket vendor makes with obs-websocket.
 */

#include <functional>

#include <QAbstractButton>
#include <QAbstractSpinBox>
#include <QApplication>
#include <QBrush>
#include <QColor>
#include <QDateTime>
#include <QDialog>
#include <QDialogButtonBox>
#include <QFont>
#include <QFontDatabase>
#include <QFontMetrics>
#include <QFormLayout>
#include <QFrame>
#include <QHBoxLayout>
#include <QHeaderView>
#include <QLabel>
#include <QLineEdit>
#include <QPalette>
#include <QMouseEvent>
#include <QPaintEvent>
#include <QPainter>
#include <QPen>
#include <QSpinBox>
#include <QStyle>
#include <QStyledItemDelegate>
#include <QTableWidget>
#include <QTimer>
#include <QVariantAnimation>
#include <QVBoxLayout>
#include <QWidget>

#include "irl-sync-dock.h"

#ifdef _WIN32
#include <windows.h>
#else
#include <dlfcn.h>
#endif


/* ── The plugin, through its function table ───────────────── */

/* Filled by irl_sync_dock_register() and read by everything below. The
 * wrappers keep the names the C plugin's dock called, so the widget code is
 * the same code. */
static struct irl_sync_dock_api g_api = {};

#define IRL_SYNC_MIN_OFFSET_MS (g_api.min_offset_ms)
#define IRL_SYNC_MAX_OFFSET_MS (g_api.max_offset_ms)
#define IRL_SYNC_DEFAULT_NTP_SERVER (g_api.default_ntp_server)

static void irl_sync_config_get(struct irl_sync_config *out)
{
	g_api.config_get(out);
}

static void irl_sync_set_enabled(bool enabled)
{
	g_api.set_enabled(enabled);
}

static void irl_sync_set_offset_ms(int offset_ms)
{
	g_api.set_offset_ms(offset_ms);
}

static void irl_sync_set_ntp_server(const char *host)
{
	g_api.set_ntp_server(host);
}

static int irl_sync_offset_ms(void)
{
	struct irl_sync_config cfg;
	g_api.config_get(&cfg);
	return cfg.offset_ms;
}

static void irl_ntp_get_status(struct irl_ntp_status *out)
{
	g_api.ntp_status(out);
}

static bool irl_ntp_utc_now_ns(int64_t *utc_ns)
{
	return g_api.utc_now_ns(utc_ns);
}

static size_t irl_sync_collect(struct irl_sync_entry *entries, size_t max)
{
	return g_api.collect(entries, max);
}

static void irl_dock_log(enum irl_dock_log_level level, const char *message)
{
	if (g_api.log)
		g_api.log(level, message);
}

#define IRL_SYNC_DOCK_ID "irl-sync-dock"

/* Several missed polls (the client refreshes about once a minute). Past this
 * the offset is old enough that everything derived from it is suspect. */
static constexpr quint64 NTP_STALE_AGE_NS = 300ULL * 1000000000ULL;

/* The clock is redrawn every tick so the milliseconds read as genuinely live
 * rather than stepping; the table and the alarm blink are decimated from it,
 * since neither benefits from more than a few updates a second. */
static constexpr int TICK_MS = 50;
static constexpr int TABLE_EVERY = 4;  /* ~5Hz */
static constexpr int BLINK_EVERY = 12; /* toggles every ~600ms */

/* Panel palette. Fixed rather than theme-derived on purpose: the clock is a
 * self-contained readout that paints its own dark ground, so it looks the same
 * under every OBS theme — the way a hardware timecode display does. */
static constexpr const char *PANEL_BG = "#0d1117";
/* The two clock faces sit on their own ground, a shade above the panel, so each
 * reads as one instrument rather than as four stacked lines of text. */
static constexpr const char *PANEL_CARD = "#161b22";
static constexpr const char *PANEL_BORDER = "#30363d";
static constexpr const char *PANEL_DIM = "#7d8590";
static constexpr const char *PANEL_LIVE = "#3fb950";
static constexpr const char *PANEL_LIVE_DIM = "#2ea043";
static constexpr const char *PANEL_DEAD = "#484f58";
static constexpr const char *PANEL_FAULT = "#f85149";
/* The offset badge. Amber rather than green: it is the one number here the
 * operator sets, so it should not blend into the readouts that only report. */
static constexpr const char *PANEL_WARN = "#e3b341";
static constexpr const char *PANEL_WARN_BG = "#3a2c0b";

/* Height of the clock digits, in pixels. Fixed rather than derived from the UI
 * font, for the same reason the palette above is fixed: this is an instrument
 * face, and an instrument's digits are the size they are. Everything else in
 * the panel keeps the theme's font, so this gap is also what marks the digits
 * out as the thing to look at.
 *
 * It goes through the label's own stylesheet, never QWidget::setFont, and the
 * rule carries an ID selector. setFont loses outright — a font-size in the OBS
 * theme's application stylesheet overrides it, which left the digits at the
 * theme's size — and a bare declaration in a widget stylesheet loses to any
 * themed rule more specific than a type selector. An ID rule on the widget
 * itself outranks both. */
static constexpr int DIGIT_PX = 30;

/* ── Frontend entry points ────────────────────────────────── */

typedef bool (*add_dock_by_id_fn)(const char *id, const char *title,
				  void *widget);
typedef void (*remove_dock_fn)(const char *id);

static void *resolve_frontend_symbol(const char *name)
{
#ifdef _WIN32
	HMODULE module = GetModuleHandleA("obs-frontend-api.dll");
	if (!module)
		return nullptr;
	return reinterpret_cast<void *>(GetProcAddress(module, name));
#else
	/* RTLD_DEFAULT searches everything already loaded, which is where
	 * obs-frontend-api is: the UI links it before any plugin loads. */
	return dlsym(RTLD_DEFAULT, name);
#endif
}

/* ── Formatting ───────────────────────────────────────────── */

/* Every number in this dock changes in place, so all of them are drawn in the
 * fixed-pitch face. QFont::setStyleHint(QFont::Monospace) is only a hint and
 * leaves the proportional UI family in place, which makes digits jump
 * sideways as they tick — asking QFontDatabase for the real fixed font is what
 * actually holds them still. */
static QFont fixedFont(const QWidget *w, int pointDelta = 0, bool bold = false)
{
	QFont f = QFontDatabase::systemFont(QFontDatabase::FixedFont);
	f.setPointSize(w->font().pointSize() + pointDelta);
	f.setBold(bold);
	return f;
}

/* The fixed-pitch family by name, so it can be named in a stylesheet. Asking
 * for it by family is the whole point: QFont::setStyleHint(QFont::Monospace) is
 * a hint the theme's proportional family satisfies, and proportional digits
 * jump sideways as they tick. */
static QString fixedFamily()
{
	return QFontDatabase::systemFont(QFontDatabase::FixedFont).family();
}

/* One clock face, whole. The milliseconds are the same size and weight as the
 * seconds: a clock reads as a clock because its digits are one run, and setting
 * part of it smaller turns the readout into a number with a footnote.
 *
 * The digits are the only thing in this panel that leaves the theme's font.
 * They have to: they change in place, and a proportional face makes them shift
 * sideways every tick. Everything else around them is ordinary UI text and is
 * left in whatever the running OBS theme uses. */
static QString statusStyle(const char *colour)
{
	return QString("#irlClockStatus { color: %1; background: transparent;"
		       " border: none; }")
		.arg(colour);
}

static QString digitStyle(const char *colour)
{
	return QString("#irlClockDigits { color: %1; background: transparent;"
		       " border: none; font-family: \"%2\";"
		       " font-size: %3px; font-weight: bold; }")
		.arg(colour, fixedFamily())
		.arg(DIGIT_PX);
}

/* Character counts every measured cell is padded out to, and every measured
 * column is sized from. Both are worst cases, not typical ones, so a value can
 * never widen its column and a column can never crop its value:
 *
 *   VALUE_CHARS  "-1800.00 s" — format_ms at the ±30 minute wrap that
 *                timecode_latency_ns folds arrival latency into.
 *   TC_CHARS     "HH:MM:SS:FFF" — n_frames is a 9-bit field, so three digits
 *                are legal even though 60fps senders never pass two.
 *
 * Padding as well as fixing the width matters: the cells are centred, so
 * without it a shorter string would still slide around inside a column that
 * itself was not moving. */
static constexpr int VALUE_CHARS = 10;
static constexpr int TC_CHARS = 12;
/* Drift is the one figure that does not need the full width: it is a value in
 * milliseconds in every case anyone watches it for, so it is padded and sized
 * for "-999 ms" with one character to spare rather than for the ±30 minute
 * worst case the others carry. A drift wide enough to overrun this is a source
 * whose status column is already saying something more useful. */
static constexpr int DRIFT_CHARS = 8;

/* Slack added to a measured column so the text is not flush against the cell
 * edge, and so a theme with roomier cell margins still clears the value. */
static constexpr int CELL_PADDING_PX = 20;

static QString pad(const QString &text, int chars)
{
	return text.rightJustified(chars, QChar(' '));
}

static QString format_ms(int64_t ms)
{
	if (ms > -1000 && ms < 1000)
		return QString("%1 ms").arg(ms);
	return QString("%1 s").arg(static_cast<double>(ms) / 1000.0, 0, 'f', 2);
}

/* Padded variants, for the cells. The bare formatter stays for prose. */
static QString format_value(int64_t ms)
{
	return pad(format_ms(ms), VALUE_CHARS);
}

static QString no_value(int chars)
{
	return pad(QStringLiteral("—"), chars);
}

static QString format_timecode(const struct irl_sync_snapshot &snap)
{
	if (!snap.have_timecode)
		return no_value(TC_CHARS);

	return pad(QString("%1:%2:%3:%4")
			   .arg(snap.tc.hours, 2, 10, QChar('0'))
			   .arg(snap.tc.minutes, 2, 10, QChar('0'))
			   .arg(snap.tc.seconds, 2, 10, QChar('0'))
			   .arg(snap.tc.n_frames, 2, 10, QChar('0')),
		   TC_CHARS);
}

/* Colour and wording per state. The colour carries the state, so the wording
 * does not repeat it with a bullet — only the two states that want the
 * operator to do something keep a marker. Four states rather than red/green,
 * because
 * they call for different actions: "No timecode" is a sender configuration
 * problem the offset cannot fix, "Acquiring" is the normal path to Locked and
 * must not read as a fault, and only "Too slow" is something the operator can
 * act on here — so that is the one that says what to do. */
static QColor status_colour(enum irl_sync_status status)
{
	switch (status) {
	case IRL_SYNC_LOCKED:
		return QColor(63, 185, 80);
	case IRL_SYNC_ACQUIRING:
		return QColor(210, 153, 34);
	case IRL_SYNC_TOO_SLOW:
		return QColor(248, 81, 73);
	case IRL_SYNC_STALE:
		return QColor(191, 60, 53);
	case IRL_SYNC_NO_TIMECODE:
	case IRL_SYNC_OFF:
	default:
		return QColor(125, 133, 144);
	}
}

static QString status_text(const struct irl_sync_snapshot &snap)
{
	switch (snap.status) {
	case IRL_SYNC_LOCKED:
		return QStringLiteral("Locked");
	case IRL_SYNC_ACQUIRING:
		return QStringLiteral("Acquiring…");
	case IRL_SYNC_TOO_SLOW:
		return QStringLiteral("Too slow");
	case IRL_SYNC_STALE:
		return QStringLiteral("No data");
	case IRL_SYNC_NO_TIMECODE:
		/* Which of the three causes it is lives in the tooltip. The
		 * cause is the actionable half, but spelling it out here sized
		 * the column for the longest of them on every row, and the
		 * underline says there is more to read. */
		return QStringLiteral("No timecode");
	case IRL_SYNC_OFF:
	default:
		return QStringLiteral("Off");
	}
}

/* Spelled out in full, on hover. Every state that returns something here is
 * drawn with a dotted underline — see UnderlineTipDelegate — so returning a
 * tooltip is also what advertises one. */
static QString status_tooltip(const struct irl_sync_snapshot &snap)
{
	switch (snap.status) {
	case IRL_SYNC_NO_TIMECODE:
		switch (snap.tc_reason) {
		case IRL_SYNC_TC_NO_CLOCK:
			return QStringLiteral(
				"This machine has no NTP reference yet, so timecodes "
				"cannot be placed on a shared clock. Check the NTP "
				"server above.");
		case IRL_SYNC_TC_CODEC:
			return QStringLiteral(
				"SEI timecodes only exist on H.265/HEVC. Switch the "
				"sender's codec — Moblin's H.264 path does not write "
				"them at all.");
		case IRL_SYNC_TC_ABSENT:
			return QStringLiteral(
				"The stream is H.265 but carries no time_code SEI. On "
				"Moblin: Settings > Streams > (stream) > Video > "
				"Timecodes, and set an NTP pool. Timecodes only reach "
				"the wire over SRT, SRTLA or RIST — never RTMP.");
		default:
			break;
		}
		return QString();
	case IRL_SYNC_TOO_SLOW:
		/* The number is the point: it tells the operator whether to
		 * raise the offset themselves or call the person in the
		 * field. */
		return QString("This feed arrives later than the target offset, so "
			       "its frames are already past their slot. Raise the "
			       "offset to at least %1 s, or improve that uplink.")
			.arg(snap.required_offset_ms / 1000);
	case IRL_SYNC_STALE:
		return QStringLiteral("This feed has stopped delivering.");
	default:
		return QString();
	}
}

/* Widest string status_text can ever return, in pixels.
 *
 * Generated by running every state through the real formatter rather than
 * measuring a hardcoded specimen, so a reworded status cannot silently start
 * being cropped. */
static int widest_status_px(const QWidget *w)
{
	static const enum irl_sync_status statuses[] = {
		IRL_SYNC_OFF,      IRL_SYNC_NO_TIMECODE, IRL_SYNC_STALE,
		IRL_SYNC_TOO_SLOW, IRL_SYNC_ACQUIRING,   IRL_SYNC_LOCKED};
	static const enum irl_sync_tc_reason reasons[] = {
		IRL_SYNC_TC_OK, IRL_SYNC_TC_NO_CLOCK, IRL_SYNC_TC_CODEC,
		IRL_SYNC_TC_ABSENT};

	const QFontMetrics fm(w->font());
	struct irl_sync_snapshot probe = {};
	/* The one status that interpolates a number, at its largest. */
	probe.required_offset_ms = IRL_SYNC_MAX_OFFSET_MS;

	int widest = 0;
	for (enum irl_sync_status status : statuses) {
		probe.status = status;
		for (enum irl_sync_tc_reason reason : reasons) {
			probe.tc_reason = reason;
			widest = qMax(widest,
				      fm.horizontalAdvance(status_text(probe)));
		}
	}
	return widest;
}

/* ── Dock widget ──────────────────────────────────────────── */

namespace {

/* The master switch, drawn as a sliding toggle rather than a labelled button.
 *
 * It lives on the offset card's bottom line, where a line of prose used to sit.
 * That is the point: every other control in this dock is now the readout it
 * affects, and a rectangular "Sync: ON" button parked underneath the panel read
 * as a separate piece of furniture. A toggle needs no text because its position
 * is its state.
 *
 * QAbstractButton rather than QCheckBox: the checkable behaviour, the clicked
 * signal and the keyboard handling all come for free, and only the painting has
 * to be replaced. The knob slides rather than jumping, because the movement is
 * what tells you which way it just went. */
class ToggleSwitch : public QAbstractButton {
public:
	explicit ToggleSwitch(QWidget *parent = nullptr)
		: QAbstractButton(parent)
	{
		setCheckable(true);
		setCursor(Qt::PointingHandCursor);
		setFocusPolicy(Qt::NoFocus);
		setFixedSize(TRACK_W, TRACK_H);

		slide.setDuration(120);
		slide.setStartValue(0.0);
		slide.setEndValue(1.0);
		QObject::connect(&slide, &QVariantAnimation::valueChanged, this,
				 [this](const QVariant &v) {
					 position = v.toReal();
					 update();
				 });
	}

	/* Both entry points go through here so the knob is never left behind
	 * the state: clicked() flips the check, and the periodic refresh sets
	 * it from the config. */
	void setOn(bool on)
	{
		if (isChecked() != on)
			setChecked(on);

		const qreal target = on ? 1.0 : 0.0;
		if (qFuzzyCompare(position + 1.0, target + 1.0))
			return;

		slide.stop();
		slide.setStartValue(position);
		slide.setEndValue(target);
		slide.start();
	}

protected:
	void paintEvent(QPaintEvent *) override
	{
		QPainter p(this);
		p.setRenderHint(QPainter::Antialiasing);

		const qreal radius = height() / 2.0;
		p.setPen(Qt::NoPen);

		/* The track carries the state as colour, the knob as position.
		 * Either one alone would do; both is what makes it readable at
		 * a glance and still readable without colour. */
		QColor off(QString::fromLatin1(PANEL_DEAD));
		QColor on(QString::fromLatin1(PANEL_LIVE));
		QColor track(off.red() + (on.red() - off.red()) * position,
			     off.green() + (on.green() - off.green()) * position,
			     off.blue() + (on.blue() - off.blue()) * position);
		p.setBrush(track);
		p.drawRoundedRect(QRectF(0, 0, width(), height()), radius,
				  radius);

		const qreal margin = 2.0;
		const qreal size = height() - 2 * margin;
		const qreal travel = width() - 2 * margin - size;
		p.setBrush(QColor(QStringLiteral("#f0f6fc")));
		p.drawEllipse(
			QRectF(margin + travel * position, margin, size, size));
	}

private:
	static constexpr int TRACK_W = 34;
	static constexpr int TRACK_H = 18;

	QVariantAnimation slide;
	qreal position = 0.0;
};

/* A label that opens something when clicked.
 *
 * The two settings this dock has are the NTP server and the offset, and both
 * are already on screen as readouts. Putting a control next to a readout of the
 * same number says the same thing twice, so the readout is the control: click
 * the value you want to change. The pointing cursor is what advertises it,
 * which is why the handler and the cursor are set together and never
 * separately. */
class ClickableLabel : public QLabel {
public:
	using QLabel::QLabel;

	void setClickHandler(std::function<void()> handler)
	{
		onClick = std::move(handler);
		setCursor(onClick ? Qt::PointingHandCursor : Qt::ArrowCursor);
	}

protected:
	void mouseReleaseEvent(QMouseEvent *event) override
	{
		/* On release rather than press, and only inside the label, so a
		 * click begun elsewhere and dragged on does not fire — the same
		 * rule every button follows. */
		if (event->button() == Qt::LeftButton && onClick &&
		    rect().contains(event->pos()))
			onClick();
		QLabel::mouseReleaseEvent(event);
	}

private:
	std::function<void()> onClick;
};

/* Marks a cell whose text is the short form of something longer by underlining
 * it with dots, the way a printed glossary term is marked.
 *
 * A tooltip nobody knows is there is a tooltip nobody reads, and the status
 * column now says "No timecode" where it used to say which of the three causes
 * it was — so the cell has to advertise that the rest of it is one hover away.
 * Keyed off the tooltip itself rather than a separate flag, so a state can
 * never gain a tooltip without gaining the underline that points at it. */
class UnderlineTipDelegate : public QStyledItemDelegate {
public:
	using QStyledItemDelegate::QStyledItemDelegate;

	void paint(QPainter *painter, const QStyleOptionViewItem &option,
		   const QModelIndex &index) const override
	{
		QStyledItemDelegate::paint(painter, option, index);

		if (index.data(Qt::ToolTipRole).toString().isEmpty())
			return;

		const QString text = index.data(Qt::DisplayRole).toString();
		if (text.isEmpty())
			return;

		QStyleOptionViewItem opt = option;
		initStyleOption(&opt, index);

		const QStyle *style = opt.widget ? opt.widget->style()
						 : QApplication::style();
		const QRect box = style->subElementRect(
			QStyle::SE_ItemViewItemText, &opt, opt.widget);

		/* Under the text, not under the cell: the line has to be as
		 * wide as the words to read as an underline rather than as a
		 * border. */
		const QFontMetrics fm(opt.font);
		const int width = qMin(fm.horizontalAdvance(text), box.width());
		int x = box.left();
		if (opt.displayAlignment & Qt::AlignHCenter)
			x += (box.width() - width) / 2;
		else if (opt.displayAlignment & Qt::AlignRight)
			x = box.right() - width;

		const int baseline = box.top() + (box.height() + fm.ascent() -
						  fm.descent()) / 2;

		QPen pen(opt.palette.color(QPalette::Text));
		const QVariant fg = index.data(Qt::ForegroundRole);
		if (fg.isValid())
			pen.setColor(fg.value<QBrush>().color());
		pen.setStyle(Qt::DotLine);

		painter->save();
		painter->setPen(pen);
		painter->drawLine(x, baseline + 2, x + width, baseline + 2);
		painter->restore();
	}
};

/* Only sources with Sync ticked are listed, so there is no column saying
 * whether they are: every row is one. */
enum column {
	COL_SOURCE = 0,
	COL_TIMECODE,
	COL_LATENCY,
	COL_ADDED,
	COL_DRIFT,
	COL_STATUS,
	COL_COUNT,
};

class SyncDock : public QWidget {
public:
	explicit SyncDock(QWidget *parent = nullptr) : QWidget(parent)
	{
		build();

		auto *timer = new QTimer(this);
		QObject::connect(timer, &QTimer::timeout, this,
				 [this]() { tick(); });
		timer->start(TICK_MS);
	}

private:
	ClickableLabel *serverCaption = nullptr;
	QLabel *clockLabel = nullptr;
	QLabel *ntpLabel = nullptr;
	ClickableLabel *offsetCaption = nullptr;
	ClickableLabel *offsetBadge = nullptr;
	QLabel *showingLabel = nullptr;
	QLabel *showingCaption = nullptr;
	QLabel *summaryLabel = nullptr;
	ToggleSwitch *masterSwitch = nullptr;
	QTableWidget *table = nullptr;

	int ticks = 0;
	bool blinkOn = false;
	void build()
	{
		auto *root = new QVBoxLayout(this);
		root->setContentsMargins(12, 12, 12, 12);
		root->setSpacing(10);

		root->addWidget(buildClockPanel());

		summaryLabel = new QLabel(this);
		summaryLabel->setAlignment(Qt::AlignCenter);
		summaryLabel->hide();
		root->addWidget(summaryLabel);

		buildTable();
		root->addWidget(table, 1);
	}

	/* One clock face. Caption on top, digits, a status line under them —
	 * the same three rows on both sides, so the pair reads as two of the
	 * same instrument showing two different times rather than as a heading
	 * with a footnote. */
	QFrame *buildClockCard(QWidget *parent, ClickableLabel **caption,
			       QLabel **digits, QLabel **status)
	{
		auto *card = new QFrame(parent);
		card->setObjectName(QStringLiteral("irlClockCard"));

		auto *box = new QVBoxLayout(card);
		box->setContentsMargins(14, 9, 14, 9);
		box->setSpacing(2);

		*caption = new ClickableLabel(card);
		(*caption)->setObjectName(QStringLiteral("irlClockCaption"));
		(*caption)->setTextFormat(Qt::RichText);
		(*caption)->setStyleSheet(QStringLiteral(
			"#irlClockCaption { background: transparent;"
			" border: none; font-weight: bold; }"));

		/* Plain text, so nothing about the digits is negotiable: rich
		 * text would let a stray tag resize part of the face. */
		*digits = new QLabel(card);
		(*digits)->setObjectName(QStringLiteral("irlClockDigits"));
		(*digits)->setTextFormat(Qt::PlainText);
		(*digits)->setStyleSheet(digitStyle(PANEL_DEAD));
		/* Reserve the height the digits will need so the panel does not
		 * resize the moment the clock goes from placeholder to live.
		 * Measured from the same spec the stylesheet sets, because the
		 * widget font is not what draws them any more. */
		QFont probe(fixedFamily());
		probe.setPixelSize(DIGIT_PX);
		probe.setBold(true);
		(*digits)->setMinimumHeight(QFontMetrics(probe).height() + 4);

		*status = new QLabel(card);
		(*status)->setObjectName(QStringLiteral("irlClockStatus"));
		(*status)->setTextFormat(Qt::RichText);
		(*status)->setStyleSheet(statusStyle(PANEL_DIM));

		box->addWidget(*caption);
		box->addWidget(*digits);
		box->addWidget(*status);

		return card;
	}

	QWidget *buildClockPanel()
	{
		auto *panel = new QFrame(this);
		panel->setObjectName(QStringLiteral("irlClockPanel"));
		panel->setStyleSheet(
			QString("#irlClockPanel { background-color: %1;"
				" border: 1px solid %2; border-radius: 8px; }"
				" QFrame#irlClockCard { background-color: %3;"
				" border: 1px solid %2; border-radius: 6px; }"
				/* A theme that paints QWidget backgrounds
				 * would otherwise draw a box behind every line
				 * inside the cards. The badge sets its own
				 * stylesheet, which outranks this. */
				" QFrame#irlClockCard QLabel {"
				" background: transparent; border: none; }")
				.arg(PANEL_BG, PANEL_BORDER, PANEL_CARD));

		auto *row = new QHBoxLayout(panel);
		row->setContentsMargins(10, 10, 10, 10);
		row->setSpacing(10);

		/* Left: the reference. Named by its server rather than labelled
		 * "NTP time", because which server it is is the thing that has
		 * to match the senders, and the zone is spelled out because the
		 * timecodes in the table arrive stamped in UTC too. */
		row->addWidget(buildClockCard(panel, &serverCaption, &clockLabel,
					      &ntpLabel),
			       1);
		serverCaption->setClickHandler([this]() { openServerDialog(); });
		serverCaption->setToolTip(
			QStringLiteral("Change the NTP server"));

		/* Right: what is on screen right now. Without it the reference
		 * clock and the source timecodes differ by exactly the offset
		 * and look like a bug. */
		auto *right = buildClockCard(panel, &offsetCaption, &showingLabel,
					     &showingCaption);

		/* The offset rides in the right caption, as a badge: it is the
		 * difference between the two faces, so it belongs on the face
		 * that is behind rather than on a line of its own. */
		offsetBadge = new ClickableLabel(right);
		offsetBadge->setObjectName(QStringLiteral("irlOffsetBadge"));
		offsetBadge->setClickHandler([this]() { openOffsetDialog(); });
		offsetBadge->setToolTip(QStringLiteral("Change the offset"));
		offsetBadge->setStyleSheet(
			QString("#irlOffsetBadge { color: %1;"
				" background-color: %2; border-radius: 4px;"
				" padding: 1px 6px; font-weight: bold; }")
				.arg(PANEL_WARN, PANEL_WARN_BG));

		auto *head = new QHBoxLayout();
		head->setContentsMargins(0, 0, 0, 0);
		head->setSpacing(6);
		head->addWidget(offsetCaption);
		head->addStretch(1);
		head->addWidget(offsetBadge);

		auto *rightBox = qobject_cast<QVBoxLayout *>(right->layout());
		rightBox->removeWidget(offsetCaption);
		rightBox->insertLayout(0, head);

		/* The master switch takes the bottom line of this card, where
		 * the status prose was. It belongs on this face rather than the
		 * reference clock: the reference ticks whether sync is on or
		 * not, and this is the one that goes grey when it is off. */
		masterSwitch = new ToggleSwitch(right);
		QObject::connect(masterSwitch, &QAbstractButton::clicked, this,
				 [this](bool checked) {
					 irl_sync_set_enabled(checked);
					 updateMasterSwitch(checked);
				 });

		auto *masterLabel = new QLabel(QStringLiteral("Sync on/off"),
					       right);
		masterLabel->setObjectName(QStringLiteral("irlClockStatus"));
		masterLabel->setStyleSheet(statusStyle(PANEL_DIM));

		/* Right, against the offset badge above it, so the two things
		 * on this card that are controls rather than readouts line up
		 * down the same edge. The status text keeps the left, where the
		 * matching line on the reference card is. */
		auto *foot = new QHBoxLayout();
		foot->setContentsMargins(0, 0, 0, 0);
		foot->setSpacing(8);
		foot->addWidget(showingCaption);
		foot->addStretch(1);
		foot->addWidget(masterLabel);
		foot->addWidget(masterSwitch);

		rightBox->removeWidget(showingCaption);
		rightBox->addLayout(foot);

		row->addWidget(right, 1);

		return panel;
	}

	void buildTable()
	{
		table = new QTableWidget(0, COL_COUNT, this);
		table->setHorizontalHeaderLabels(
			{QStringLiteral("Source"), QStringLiteral("Timecode"),
			 QStringLiteral("Latency"), QStringLiteral("Added"),
			 QStringLiteral("Drift"), QStringLiteral("Status")});
		table->verticalHeader()->setVisible(false);
		table->verticalHeader()->setDefaultSectionSize(
			QFontMetrics(font()).height() + 12);
		table->setSelectionMode(QAbstractItemView::NoSelection);
		table->setEditTriggers(QAbstractItemView::NoEditTriggers);
		table->setFocusPolicy(Qt::NoFocus);
		table->setAlternatingRowColors(true);
		/* Row striping carries the eye across; grid lines on top of it
		 * just add noise at this column count. */
		table->setShowGrid(false);
		table->horizontalHeader()->setHighlightSections(false);

		QFont headerFont = table->horizontalHeader()->font();
		headerFont.setBold(true);
		table->horizontalHeader()->setFont(headerFont);

		/* Every measured column is nailed to the width of its widest
		 * possible value.
		 *
		 * ResizeToContents is what these used to be, and it is wrong
		 * here for a reason worth recording: the values tick several
		 * times a second, so the column tracked them, and because a
		 * column resize shifts every column after it, one digit
		 * appearing in Error moved the whole right-hand side of the
		 * table. A readout that jitters as it updates is harder to read
		 * than one that is a few pixels wider than it needs to be. */
		auto fix = [&](int column, int chars) {
			const QFontMetrics values(fixedFont(this));
			const QFontMetrics header(
				table->horizontalHeader()->font());
			/* The header is bold and in the UI face, so a short
			 * column can be titled wider than anything it holds. */
			const QTableWidgetItem *title =
				table->horizontalHeaderItem(column);
			const int width =
				qMax(values.horizontalAdvance(
					     QString(chars, QChar('0'))),
				     title ? header.horizontalAdvance(
						     title->text())
					   : 0) +
				CELL_PADDING_PX;
			table->horizontalHeader()->setSectionResizeMode(
				column, QHeaderView::Fixed);
			table->setColumnWidth(column, width);
		};

		fix(COL_TIMECODE, TC_CHARS);
		fix(COL_LATENCY, VALUE_CHARS);
		fix(COL_ADDED, VALUE_CHARS);
		fix(COL_DRIFT, DRIFT_CHARS);

		/* Status is prose rather than a figure, so it is sized from the
		 * longest wording instead of a character count, and left
		 * resizable because it is the one column someone might want to
		 * reclaim space from. Interactive never resizes itself, so it
		 * does not reintroduce the jitter. */
		table->setItemDelegateForColumn(COL_STATUS,
						new UnderlineTipDelegate(table));

		table->horizontalHeader()->setSectionResizeMode(
			COL_STATUS, QHeaderView::Interactive);
		table->setColumnWidth(COL_STATUS,
				      widest_status_px(this) + CELL_PADDING_PX);

		/* Last, so the stretch absorbs whatever the fixed columns
		 * leave rather than fighting them for it. */
		table->horizontalHeader()->setSectionResizeMode(
			COL_SOURCE, QHeaderView::Stretch);
	}

	/* Opened by clicking the server name on the reference clock. Modal and
	 * apply-on-OK: this is set once when the setup is built and then left
	 * alone, so it is worth a deliberate confirmation. */
	void openServerDialog()
	{
		QDialog dialog(this);
		dialog.setWindowTitle(QStringLiteral("NTP Server"));

		auto *box = new QVBoxLayout(&dialog);
		auto *form = new QFormLayout();

		struct irl_sync_config cfg;
		irl_sync_config_get(&cfg);

		auto *server = new QLineEdit(QString::fromUtf8(cfg.ntp_server),
					     &dialog);
		server->setPlaceholderText(
			QString::fromUtf8(IRL_SYNC_DEFAULT_NTP_SERVER));
		server->setMinimumWidth(220);
		form->addRow(QStringLiteral("NTP server"), server);
		box->addLayout(form);

		/* The one setting that has to match on equipment this dock
		 * cannot see, which is exactly why it needs saying here. */
		auto *note = new QLabel(
			QStringLiteral(
				"Must be a reference the senders also use.\n"
				"Set the same pool here and in each sending device."),
			&dialog);
		note->setStyleSheet(QString("color: %1;").arg(PANEL_DIM));
		box->addWidget(note);

		auto *buttons = new QDialogButtonBox(
			QDialogButtonBox::Ok | QDialogButtonBox::Cancel,
			&dialog);
		QObject::connect(buttons, &QDialogButtonBox::accepted, &dialog,
				 &QDialog::accept);
		QObject::connect(buttons, &QDialogButtonBox::rejected, &dialog,
				 &QDialog::reject);
		box->addWidget(buttons);

		if (dialog.exec() != QDialog::Accepted)
			return;

		irl_sync_set_ntp_server(
			server->text().trimmed().toUtf8().constData());
	}

	/* Opened by clicking the offset badge. Modal and apply-on-OK for a
	 * different reason than the server dialog: changing the offset is a
	 * several-second step for every synced feed at once, so it should
	 * happen when someone says so, not while they are still typing. */
	void openOffsetDialog()
	{
		QDialog dialog(this);
		dialog.setWindowTitle(QStringLiteral("Sync Offset"));

		auto *box = new QVBoxLayout(&dialog);
		auto *form = new QFormLayout();

		/* Whole seconds. See IRL_SYNC_OFFSET_STEP_MS: this is the
		 * number co-streamers agree on out loud, so the control offers
		 * exactly the values that are worth saying. */
		auto *spin = new QSpinBox(&dialog);
		spin->setRange(IRL_SYNC_MIN_OFFSET_MS / 1000,
			       IRL_SYNC_MAX_OFFSET_MS / 1000);
		spin->setSuffix(QStringLiteral(" s"));
		spin->setValue(irl_sync_offset_ms() / 1000);
		spin->setFont(fixedFont(this));
		form->addRow(QStringLiteral("Offset"), spin);
		box->addLayout(form);

		auto *note = new QLabel(
			QStringLiteral(
				"How far behind live every synced source plays.\n"
				"Everyone sharing a shot needs the same number,\n"
				"and it has to clear the slowest feed's latency."),
			&dialog);
		note->setStyleSheet(QString("color: %1;").arg(PANEL_DIM));
		box->addWidget(note);

		auto *buttons = new QDialogButtonBox(
			QDialogButtonBox::Ok | QDialogButtonBox::Cancel,
			&dialog);
		QObject::connect(buttons, &QDialogButtonBox::accepted, &dialog,
				 &QDialog::accept);
		QObject::connect(buttons, &QDialogButtonBox::rejected, &dialog,
				 &QDialog::reject);
		box->addWidget(buttons);

		if (dialog.exec() != QDialog::Accepted)
			return;

		irl_sync_set_offset_ms(spin->value() * 1000);
	}

	void updateMasterSwitch(bool enabled)
	{
		masterSwitch->setOn(enabled);
		/* The switch carries no text, so the words it replaced live
		 * here. */
		masterSwitch->setToolTip(
			enabled ? QStringLiteral("Timecode sync is on")
				: QStringLiteral("Timecode sync is off"));
	}

	QTableWidgetItem *cell(int row, int column)
	{
		QTableWidgetItem *item = table->item(row, column);
		if (!item) {
			item = new QTableWidgetItem();
			if (column == COL_SOURCE)
				item->setTextAlignment(Qt::AlignLeft |
						       Qt::AlignVCenter);
			else if (column == COL_STATUS)
				item->setTextAlignment(Qt::AlignLeft |
						       Qt::AlignVCenter);
			else
				item->setTextAlignment(Qt::AlignCenter);

			/* The measured columns tick in place; the fixed face
			 * keeps them from shifting under the header. */
			if (column >= COL_TIMECODE && column <= COL_DRIFT)
				item->setFont(fixedFont(this));
			table->setItem(row, column, item);
		}
		return item;
	}

	void tick()
	{
		struct irl_sync_config cfg;
		irl_sync_config_get(&cfg);

		refreshClock(cfg);

		if (++ticks % BLINK_EVERY == 0)
			blinkOn = !blinkOn;
		if (ticks % TABLE_EVERY == 0) {
			refreshControls(cfg);
			refreshTable();
		}
	}

	void refreshClock(const struct irl_sync_config &cfg)
	{
		struct irl_ntp_status ntp;
		irl_ntp_get_status(&ntp);

		int64_t utc_ns = 0;
		const bool live = irl_ntp_utc_now_ns(&utc_ns);

		/* UTC, not local. The reference is UTC and the senders stamp
		 * their timecodes in it, so rendering these two faces in local
		 * time put the one clock in this dock that is not on the same
		 * zone as the table under it. (toUTC() rather than a QTimeZone
		 * overload: it reads the same on every Qt 6 the plugin might be
		 * built against.) */
		QString now = QStringLiteral("--:--:--.---");
		QString showing = now;
		if (live) {
			const qint64 ms = utc_ns / 1000000LL;
			const QString fmt = QStringLiteral("HH:mm:ss.zzz");
			now = QDateTime::fromMSecsSinceEpoch(ms).toUTC().toString(
				fmt);
			showing = QDateTime::fromMSecsSinceEpoch(ms -
								 cfg.offset_ms)
					  .toUTC()
					  .toString(fmt);
		}

		clockLabel->setText(now);
		clockLabel->setStyleSheet(
			digitStyle(live ? PANEL_LIVE : PANEL_DEAD));

		/* The reference clock is live whenever there is a reference,
		 * but nothing is being delayed unless sync is on, so this face
		 * goes grey with the master switch rather than reading as a
		 * time something is actually being shown at. */
		showingLabel->setText(showing);
		showingLabel->setStyleSheet(digitStyle(
			live && cfg.enabled ? PANEL_LIVE : PANEL_DEAD));

		offsetBadge->setText(
			QString("−%1").arg(format_ms(cfg.offset_ms)));

		if (live) {
			/* Nothing to say: the switch beside this line already
			 * says sync is on, and the two faces above say what it
			 * is doing. Prose here would only repeat them. */
			showingCaption->setText(QString());
		} else {
			/* Not being synced is only a problem if sync is meant
			 * to be running. Polling is gated on the master switch,
			 * so before it is on there is nothing wrong to report. */
			showingCaption->setText(
				cfg.enabled
					? QStringLiteral(
						  "no clock reference — sync cannot run")
					: QStringLiteral(
						  "turn sync on to start the clock"));
			showingCaption->setStyleSheet(statusStyle(
				cfg.enabled ? PANEL_FAULT : PANEL_DIM));
		}

		/* An unreachable server keeps the last offset ticking along
		 * plausibly, so freshness is reported, not just "synced". */
		QString health;
		bool fault = false;
		if (!ntp.server[0]) {
			health = QStringLiteral("no server configured");
			fault = cfg.enabled;
		} else if (!ntp.synced) {
			health = cfg.enabled
					 ? QStringLiteral("not synced")
					 : QStringLiteral("idle — polls while sync is on");
			fault = cfg.enabled;
		} else {
			/* Half the round trip, because what the reader is
			 * being told is how far the offset could be out,
			 * not how long the packet was on the wire. A
			 * decimal below 10ms: a server two milliseconds
			 * away would otherwise round to the same ±1 ms as
			 * one three times worse. */
			const double half_ms = ntp.rtt_ns / 2.0 / 1e6;
			health = QString("synced %1s ago · ±%2 ms")
					 .arg(ntp.age_ns / 1000000000ULL)
					 .arg(half_ms, 0, 'f',
					      half_ms < 10.0 ? 1 : 0);
			/* The crystal error is the other half of whether
			 * this clock can be trusted between polls: a
			 * machine 40 ppm fast moves 2.4ms every minute,
			 * and on a burst server that is what the client is
			 * correcting for rather than waiting out. */
			if (ntp.drift_valid) {
				health += QStringLiteral(" · ") +
					  QString::asprintf("%+.1f ppm",
							    ntp.drift_ppm);
			}
			/* Working, but not off the server that was asked
			 * for, and the name above this line has quietly
			 * changed to say so. */
			if (ntp.fallback_active)
				health += QStringLiteral(" · fallback");
			/* A stale reference keeps ticking plausibly while
			 * being wrong, so it reads as a fault too. */
			fault = cfg.enabled && ntp.age_ns > NTP_STALE_AGE_NS;
		}

		serverCaption->setText(
			ntp.server[0]
				? QString("<span style='color:%1;'>%2</span>"
					  "<span style='color:%3;'> · UTC</span>")
					  .arg(PANEL_LIVE,
					       QString::fromUtf8(ntp.server)
						       .toUpper(),
					       PANEL_LIVE_DIM)
				: QString("<span style='color:%1;'>NO SERVER</span>")
					  .arg(PANEL_DEAD));

		offsetCaption->setText(
			QString("<span style='color:%1;'>OFFSET</span>")
				.arg(PANEL_DIM));

		/* The server itself is the caption above the digits now, so
		 * this line carries only how healthy it is. */
		ntpLabel->setText(health);
		ntpLabel->setStyleSheet(statusStyle(fault ? PANEL_FAULT
							  : PANEL_DIM));
	}

	/* The offset needs no adopting here any more: the badge is redrawn from
	 * the live config every tick, so a change made anywhere else — a scene
	 * collection load, the websocket — shows up on its own. */
	void refreshControls(const struct irl_sync_config &cfg)
	{
		updateMasterSwitch(cfg.enabled);
	}

	void refreshTable()
	{
		struct irl_sync_entry entries[IRL_SYNC_MAX_SOURCES];
		const size_t collected = irl_sync_collect(
			entries, sizeof(entries) / sizeof(entries[0]));

		/* Sources that have not opted in are not part of the group and
		 * have nothing to report, so they are left out entirely rather
		 * than listed as a screenful of dashes. */
		int count = 0;
		for (size_t i = 0; i < collected; i++) {
			if (entries[i].sync_enabled)
				entries[count++] = entries[i];
		}

		if (table->rowCount() != count)
			table->setRowCount(count);

		for (int row = 0; row < count; row++) {
			const struct irl_sync_entry &e = entries[row];
			const struct irl_sync_snapshot &s = e.snap;

			cell(row, COL_SOURCE)
				->setText(QString::fromUtf8(e.source_name));
			cell(row, COL_TIMECODE)->setText(format_timecode(s));

			/* Both of these describe what is arriving, not what
			 * sync is doing with it, so they stay on display while
			 * sync is off. Only the two columns that report the
			 * controller's own work go blank. */
			const bool measured = s.have_timecode;
			const bool aligning = s.status == IRL_SYNC_LOCKED ||
					      s.status == IRL_SYNC_ACQUIRING;
			cell(row, COL_LATENCY)
				->setText(measured ? format_value(s.latency_ms)
						   : no_value(VALUE_CHARS));
			cell(row, COL_ADDED)
				->setText(aligning ? format_value(s.added_ms)
						   : no_value(VALUE_CHARS));
			cell(row, COL_DRIFT)
				->setText(aligning
						  ? pad(format_ms(s.error_ms),
							DRIFT_CHARS)
						  : no_value(DRIFT_CHARS));

			QTableWidgetItem *status = cell(row, COL_STATUS);
			status->setText(status_text(s));
			status->setForeground(status_colour(s.status));
			const QString tip = status_tooltip(s);
			status->setToolTip(tip);
			cell(row, COL_SOURCE)->setToolTip(tip);

			const bool alarm = s.status == IRL_SYNC_TOO_SLOW ||
					   s.status == IRL_SYNC_STALE;
			if (alarm) {
				status->setBackground(
					blinkOn ? QBrush(QColor(248, 81, 73,
								70))
						: QBrush(Qt::NoBrush));
			} else {
				status->setBackground(QBrush(Qt::NoBrush));
			}

		}

		if (count == 0) {
			/* An empty table otherwise reads as "the dock is
			 * broken" rather than "nothing has opted in". */
			summaryLabel->setText(QStringLiteral(
				"No sources have Sync enabled — tick Sync in a source's properties."));
			summaryLabel->setStyleSheet(
				QString("color: %1;").arg(PANEL_DIM));
			summaryLabel->show();
		} else {
			summaryLabel->hide();
		}
	}
};

} // namespace

/* ── Registration ─────────────────────────────────────────── */

static bool dock_added = false;

extern "C" void irl_sync_dock_abi_sizes(struct irl_sync_dock_abi *out)
{
	out->timecode = sizeof(struct irl_timecode);
	out->snapshot = sizeof(struct irl_sync_snapshot);
	out->entry = sizeof(struct irl_sync_entry);
	out->config = sizeof(struct irl_sync_config);
	out->ntp_status = sizeof(struct irl_ntp_status);
	out->api = sizeof(struct irl_sync_dock_api);
}

extern "C" bool irl_sync_dock_register(const struct irl_sync_dock_api *api)
{
	if (dock_added)
		return true;
	if (!api || api->abi_version != IRL_SYNC_DOCK_ABI_VERSION)
		return false;
	g_api = *api;

	auto add_dock = reinterpret_cast<add_dock_by_id_fn>(
		resolve_frontend_symbol("obs_frontend_add_dock_by_id"));
	if (!add_dock) {
		irl_dock_log(IRL_DOCK_LOG_INFO,
			     "OBS frontend not available; sync dock disabled (status is still on the websocket vendor)");
		return false;
	}

	/* Ownership passes to the frontend, which parents it into a
	 * QDockWidget and destroys it with the main window. */
	auto *dock = new SyncDock();
	dock->setWindowTitle(QStringLiteral("IRL Sync"));

	if (add_dock(IRL_SYNC_DOCK_ID, "IRL Sync", dock)) {
		dock_added = true;
		irl_dock_log(IRL_DOCK_LOG_INFO, "Sync dock registered");
		return true;
	}

	delete dock;
	irl_dock_log(IRL_DOCK_LOG_WARNING, "Failed to register sync dock");
	return false;
}

extern "C" void irl_sync_dock_unregister(void)
{
	if (!dock_added)
		return;

	/* Safe at shutdown as well as at a live unload: the frontend API
	 * checks its own callback table first and returns without doing
	 * anything once the main window is gone — which is also when it has
	 * already destroyed the widget and its refresh timer. */
	auto remove_dock = reinterpret_cast<remove_dock_fn>(
		resolve_frontend_symbol("obs_frontend_remove_dock"));
	if (remove_dock)
		remove_dock(IRL_SYNC_DOCK_ID);

	dock_added = false;
}
