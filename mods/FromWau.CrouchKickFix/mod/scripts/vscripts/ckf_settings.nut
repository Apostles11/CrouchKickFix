untyped

// CrouchKickFix - Mod Settings UI (same pattern as MovementTrainer).
// Registers entries in the EladNLG.ModSettings menu, each bound to a ConVar.

global function CrouchKickFix_Settings

void function CrouchKickFix_Settings()
{
    #if UI
        ModSettings_AddModTitle( "CrouchKickFix" )

        ModSettings_AddModCategory( "Fix" )
        ModSettings_AddEnumSetting( "ckf_enabled", "Enable CKF", ["Off", "On"] )

        ModSettings_AddModCategory( "Feedback" )
        ModSettings_AddEnumSetting( "ckf_ui_feedback", "Enable UI Feedback", ["Off", "On"] )
    #endif
}
